// SPDX-License-Identifier: GPL-2.0

//! Snapshots: queries of the in-memory table, and lookups in the btrees.

use crate::btree::bkey::{pos, spos, BkeyI, BkeySC, BkeyISnapshot, BkeyISnapshotTree, SizedBkey};
use crate::btree::iter::{BtreeIter, BtreeIterFlags, BtreeTrans, TransAttempt, TransBkey};
use crate::c;
use crate::errcode::{bch_errcode, ret_to_result_void, BchError};
use crate::fs::Fs;
use crate::snapshots::check_snapshots;
use crate::util::alloc::{flags::GFP_KERNEL, KVVec};
use crate::util::rcu;
use crate::util::vstructs::Ffi;
use crate::util::Printbuf;
use core::fmt;
use core::marker::PhantomData;

/// The in-memory snapshot table: C's array of snapshot_t, indexed by
/// U32_MAX - id. Ids descend, so a table covering a node covers its
/// ancestors. Read in place, unsafely, until it's a Rust structure.
struct Table<'g> {
    t:  *const c::snapshot_table,
    _g: PhantomData<&'g rcu::ReadGuard>,
}

impl<'g> Table<'g> {
    /// Valid while @g's section is open.
    fn get(snapshots: &c::bch_fs_snapshots, g: &'g rcu::ReadGuard) -> Self {
        let t = unsafe { rcu::dereference(&raw const snapshots.table, g) };
        Table { t, _g: PhantomData }
    }

    /// Not immutable, whatever the reference says: bch2_mark_snapshot() rewrites
    /// entries in place under table_lock, which readers don't take - RCU only
    /// covers the table being replaced. Too big to copy per lookup; atomic fields
    /// are the fix, once the table is Rust's.
    fn entry(&self, id: u32) -> Option<&'g c::snapshot_t> {
        let idx = (u32::MAX - id) as usize;
        unsafe {
            if self.t.is_null() || idx >= (*self.t).nr {
                return None;
            }
            Some(&*(&raw const (*self.t).s).cast::<c::snapshot_t>().add(idx))
        }
    }

    /// 0 for none.
    fn parent(&self, id: u32) -> u32 {
        self.entry(id).map_or(0, |s| s.parent)
    }
}

impl c::bch_fs_snapshots {
    /// The bch_fs these are in, for the C that takes it.
    fn c(&self) -> *mut c::bch_fs {
        unsafe {
            (self as *const Self).byte_sub(core::mem::offset_of!(c::bch_fs, snapshots))
                .cast::<c::bch_fs>().cast_mut()
        }
    }

    /// The filesystem these are in, whose errors they throw.
    fn fs(&self) -> core::mem::ManuallyDrop<Fs> {
        unsafe { Fs::borrow_raw(self.c()) }
    }

    pub fn parent(&self, id: u32) -> Option<u32> {
        let g = rcu::read_lock();
        match Table::get(self, &g).parent(id) {
            0      => None,
            parent => Some(parent),
        }
    }

    /// If @id is an interior node an interrupted deletion left, the live snapshot
    /// it collapses into.
    pub fn redundant_interior(&self, id: u32) -> Option<u32> {
        match unsafe { c::bch2_snapshot_redundant_interior(self.c(), id) } {
            0 => None,
            terminal => Some(terminal),
        }
    }

    /// By parent pointers, before the ancestor bitmaps are built.
    pub fn is_ancestor_early(&self, id: u32, ancestor: u32) -> bool {
        unsafe { c::bch2_snapshot_is_ancestor_early(self.c(), id, ancestor) }
    }

    pub fn has_children(&self, id: u32) -> bool {
        let g = rcu::read_lock();
        Table::get(self, &g).entry(id).is_some_and(|s| (s.children[0] | s.children[1]) != 0)
    }

    /// An error if there's no such snapshot.
    pub fn is_leaf(&self, id: u32) -> Result<bool, BchError> {
        let first_child = {
            let g = rcu::read_lock();
            Table::get(self, &g).entry(id).map(|s| s.children[0])
        };
        match first_child {
            Some(child) => Ok(child == 0),
            None        => self.fs().throw(bch_errcode::BCH_ERR_invalid_snapshot_node),
        }
    }

    /// 0 if there's no such snapshot.
    pub fn tree(&self, id: u32) -> u32 {
        let g = rcu::read_lock();
        Table::get(self, &g).entry(id).map_or(0, |s| s.tree)
    }

    pub fn root(&self, mut id: u32) -> u32 {
        let g = rcu::read_lock();
        let t = Table::get(self, &g);
        loop {
            match t.parent(id) {
                0      => return id,
                parent => id = parent,
            }
        }
    }

    pub fn id_state(&self, id: u32) -> c::snapshot_id_state {
        let g = rcu::read_lock();
        Table::get(self, &g).entry(id).map_or(c::snapshot_id_state::SNAPSHOT_ID_empty, |s| s.state)
    }

    /// Of a child of @parent; 0 for no parent. Panics if @parent isn't in the
    /// table, rather than have repair write a wrong depth.
    pub fn depth(&self, parent: u32) -> u32 {
        if parent == 0 {
            return 0;
        }
        let g = rcu::read_lock();
        let s = Table::get(self, &g).entry(parent)
            .unwrap_or_else(|| panic!("snapshot depth: parent {parent} not in the table"));
        s.depth + 1
    }

    /// For a skiplist entry: @id itself for a root, 0 for 0.
    pub fn skiplist_get(&self, id: u32) -> u32 {
        unsafe { c::bch2_snapshot_skiplist_get(self.c(), id) }
    }

    /// The lowest numbered subvolume in @root's subtree.
    pub fn oldest_subvol(&self, root: u32) -> Option<u32> {
        match unsafe { c::bch2_snapshot_oldest_subvol(self.c(), root) } {
            0 => None,
            subvol => Some(subvol),
        }
    }

    /// The unique live node claiming an edge with @id - as its parent if @parent,
    /// else as its child - other than those @s points at. None if none or several.
    pub fn table_find_edge(&self, s: &Ffi<c::bch_snapshot>, id: u32, parent: bool) -> Option<u32> {
        match unsafe { c::bch2_snapshot_table_find_edge(self.c(), s, id, parent) } {
            0 => None,
            n => Some(n),
        }
    }

    /// For node @id, about to be created.
    pub fn table_make_room(&self, id: u32) -> Result<(), BchError> {
        ret_to_result_void(unsafe { c::bch2_snapshot_table_make_room(self.c(), id) })
    }

    /// Have the table rebuilt from the btree, after repairs it doesn't track:
    /// see table_rebuild_if_needed().
    pub fn set_need_table_rebuild(&self) {
        unsafe { (*self.c()).snapshots.need_table_rebuild = true }
    }

    /// @id, or the live snapshot it collapses into if it's dead. An error if the
    /// tree is damaged on the way.
    pub fn live_descendent(&self, mut id: u32) -> Result<Option<u32>, BchError> {
        let err = {
            let g = rcu::read_lock();
            let t = Table::get(self, &g);
            loop {
                let Some(s) = t.entry(id) else {
                    break bch_errcode::BCH_ERR_invalid_snapshot_node;
                };
                if s.state == c::snapshot_id_state::SNAPSHOT_ID_live {
                    return Ok(Some(id));
                }
                // deleted nodes are single-child
                if s.children[1] != 0 {
                    break bch_errcode::BCH_ERR_snapshot_multiple_descendents;
                }
                if s.children[0] == 0 {
                    return Ok(None);
                }
                id = s.children[0];
            }
        };
        self.fs().throw(err)
    }

    /// Keys and sectors accounted to @id across the snapshotted btrees, broken
    /// down per btree into @breakdown.
    pub fn accounting_totals(&self, id: u32, breakdown: Option<&mut Printbuf>)
        -> Result<(u64, u64), BchError>
    {
        let (mut keys, mut sectors) = (0, 0);
        let out = breakdown.map_or(core::ptr::null_mut(), |b| b.as_raw() as *mut _);
        ret_to_result_void(unsafe {
            c::bch2_snapshot_accounting_totals(self.c(), id, &mut keys, &mut sectors,
                                               core::ptr::null_mut(), out)
        })?;
        Ok((keys, sectors))
    }
}

/// Whether @id is @ancestor or one of its descendants.
pub fn is_ancestor(trans: &BtreeTrans<'_>, id: u32, ancestor: u32) -> bool {
    debug_assert!(id != 0 && ancestor != 0);
    id == ancestor || unsafe { c::__bch2_snapshot_is_ancestor(trans.raw(), id, ancestor) }
}

/// If repairs asked for it.
pub fn table_rebuild_if_needed(trans: &BtreeTrans<'_>) -> Result<(), BchError> {
    if !unsafe { (*trans.fs().raw).snapshots.need_table_rebuild } {
        return Ok(());
    }
    ret_to_result_void(unsafe { c::bch2_snapshot_table_rebuild(trans.raw()) })
}

/// The snapshots that have overwritten the key at @pos in @btree: on each
/// branch, the one closest to @pos.snapshot.
pub fn overwrites(t: &TransAttempt<'_, '_>, btree: c::btree_id, pos: c::bpos)
    -> Result<KVVec<u32>, BchError>
{
    let trans: &BtreeTrans<'_> = t;
    let mut ids = KVVec::new();

    if !trans.fs().snapshots.has_children(pos.snapshot) {
        return Ok(ids);
    }

    // From the version before @pos's - in the snapshot with the next lower
    // ID; @pos.snapshot has children, so it isn't 0 - down
    let mut iter = BtreeIter::new(trans, btree, spos(pos.inode, pos.offset, pos.snapshot - 1),
                                  BtreeIterFlags::ALL_SNAPSHOTS);
    iter.for_each_reverse_norestart(t, spos(pos.inode, pos.offset, 0), |_, k| {
        let id = k.k.p.snapshot;
        if is_ancestor(trans, id, pos.snapshot) &&
           !ids.iter().any(|&i| is_ancestor(trans, id, i)) {
            ids.push(id, GFP_KERNEL)?;
        }
        Ok(())
    })?;
    Ok(ids)
}

pub fn tree_lookup(trans: &BtreeTrans<'_>, id: u32) -> Result<c::bch_snapshot_tree, BchError> {
    trans.attempt_in_progress()
        .bkey_get_val::<BkeyISnapshotTree>(c::btree_id::snapshot_trees, pos(0, id as u64),
                                           BtreeIterFlags::empty())
        .map_err(|e| if e.matches(c::ENOENT) {
            trans.fs().err(bch_errcode::BCH_ERR_ENOENT_snapshot_tree)
        } else {
            e
        })
}

/// At an unused id, queued for the commit, for the caller to fill in.
pub fn tree_create<'a, 't>(t: &TransAttempt<'a, 't>) -> Result<TransBkey<'a, 't>, BchError> {
    unsafe {
        let k = c::__bch2_snapshot_tree_create(t.raw());
        TransBkey::from_raw(t, k as *mut c::bkey_i)
    }
}

/// Whether @k is in a snapshot that doesn't exist - reported, and @k deleted
/// or its snapshot reconstructed - for the caller to skip it.
pub fn check_key_has_snapshot(
    trans: &BtreeTrans<'_>,
    iter:  &BtreeIter<'_>,
    k:     BkeySC<'_>,
) -> Result<bool, BchError> {
    if trans.fs().snapshots.id_state(k.k.p.snapshot) == c::snapshot_id_state::SNAPSHOT_ID_live {
        return Ok(false);
    }
    // SAFETY: BtreeIter holds its iterator in an UnsafeCell, and nothing else
    // has it for the call
    let iter = unsafe { &mut *iter.raw() };
    check_snapshots::check_key_has_snapshot(&trans.attempt_in_progress(), Some(iter), k)
}

/// ENOENT if there's none.
pub fn lookup(trans: &BtreeTrans<'_>, id: u32) -> Result<Ffi<c::bch_snapshot>, BchError> {
    trans.attempt_in_progress()
        .bkey_get_val::<BkeyISnapshot>(c::btree_id::snapshots, pos(0, id as u64),
                                       BtreeIterFlags::empty())
}

/// ENOENT if there's none.
pub fn lookup_key(trans: &BtreeTrans<'_>, id: u32) -> Result<BkeyI<Ffi<c::bch_snapshot>>, BchError> {
    trans.attempt_in_progress()
        .bkey_get_key::<BkeyISnapshot>(c::btree_id::snapshots, pos(0, id as u64),
                                       BtreeIterFlags::empty())
}

/// Zero padded past the fields an older key lacks.
pub fn val(k: BkeySC<'_>) -> Option<Ffi<c::bch_snapshot>> {
    BkeyISnapshot::val_copy_pad(k)
}

/// Mark node @id will_delete, its subvolume being deleted, if it's live.
pub fn node_set_deleted(t: &TransAttempt<'_, '_>, id: u32) -> Result<(), BchError> {
    let ret = unsafe { c::bch2_snapshot_node_set_deleted(t.raw(), id) };
    t.result(ret)
}

/// Put deleted node @u back in the tree, under its parent.
pub fn node_undelete<'t>(t: &TransAttempt<'_, 't>, u: &mut TransBkey<'_, 't>)
    -> Result<(), BchError>
{
    let ret = unsafe { c::bch2_snapshot_node_undelete(t.raw(), u.as_ptr() as *mut BkeyI<Ffi<c::bch_snapshot>>) };
    t.result(ret)
}

/// Move @k, in a dead snapshot, to @live_child; 0 deletes it.
pub fn delete_dead_key<'a, 't>(
    t:          &TransAttempt<'a, 't>,
    iter:       &mut c::btree_iter,
    k:          BkeySC<'_>,
    live_child: u32,
) -> Result<(), BchError> {
    let ret = unsafe { c::bch2_delete_dead_snapshot_key(t.raw(), iter, k.to_raw(), live_child) };
    t.result(ret)
}

/// The state codeword nearest @v and its Hamming distance - the first, on a
/// tie. Codewords are >= 14 bits apart, so within 6 bits the nearest is the
/// original.
pub fn state_nearest(v: u32) -> (c::bch_snapshot_state, u32) {
    let mut best = (c::bch_snapshot_state::SNAPSHOT_STATE_live, 33);
    for s in SNAPSHOT_STATES {
        let dist = (v ^ s.0).count_ones();
        if dist < best.1 {
            best = (s, dist);
        }
    }
    best
}

const SNAPSHOT_STATES: [c::bch_snapshot_state; 4] = [
    c::bch_snapshot_state::SNAPSHOT_STATE_live,
    c::bch_snapshot_state::SNAPSHOT_STATE_will_delete,
    c::bch_snapshot_state::SNAPSHOT_STATE_no_keys,
    c::bch_snapshot_state::SNAPSHOT_STATE_deleted,
];

impl c::bch_snapshot__ffi {
    pub fn parent(&self) -> u32       { self.parent.get() }
    pub fn children(&self) -> [u32; 2] { self.children.map(|c| c.get()) }
    pub fn subvol(&self) -> u32       { self.subvol.get() }
    pub fn tree(&self) -> u32         { self.tree.get() }
    pub fn depth(&self) -> u32        { self.depth.get() }

    /// 0 if the key predates the field.
    pub fn state_raw(&self) -> u32 {
        self.state.get()
    }

    /// None if the field is 0 or damaged.
    pub fn state_field(&self) -> Option<c::bch_snapshot_state> {
        SNAPSHOT_STATES.into_iter().find(|&s| s.0 == self.state_raw())
    }

    /// The state the legacy flags imply.
    pub fn state_from_flags(&self) -> c::bch_snapshot_state {
        if self.deleted_obsolete() {
            c::bch_snapshot_state::SNAPSHOT_STATE_deleted
        } else if self.no_keys_obsolete() {
            c::bch_snapshot_state::SNAPSHOT_STATE_no_keys
        } else if self.will_delete_obsolete() {
            c::bch_snapshot_state::SNAPSHOT_STATE_will_delete
        } else {
            c::bch_snapshot_state::SNAPSHOT_STATE_live
        }
    }

    /// From the legacy flags if the key predates the state field; None if the
    /// field is damaged.
    pub fn state(&self) -> Option<c::bch_snapshot_state> {
        if self.state == 0 {
            Some(self.state_from_flags())
        } else {
            self.state_field()
        }
    }

    /// And the legacy flags to match.
    pub fn set_state(&mut self, state: c::bch_snapshot_state) {
        unsafe { c::bch2_snapshot_state_set(self, state) }
    }
}

impl fmt::Display for c::bch_snapshot_state {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = crate::snapshot_states::SNAPSHOT_STATE_VALUES.iter()
            .find(|(_, v)| *v == self.0 as u64)
            .map_or("(invalid state)", |(n, _)| n);
        f.write_str(name)
    }
}

impl fmt::Display for c::bch_snapshot__ffi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        crate::printbuf_to_formatter(f, |out| unsafe { c::bch2_snapshot_to_text(out, self) })
    }
}
