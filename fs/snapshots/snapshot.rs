// SPDX-License-Identifier: GPL-2.0

//! Snapshots: queries of the in-memory table, and lookups in the btrees.

use crate::btree::bkey::{pos, spos, BkeyI, BkeySC, SizedBkey};
use crate::util::vstructs::{Validated, Whole};
use crate::btree::iter::{BtreeIter, BtreeIterFlags, BtreeTrans, TransAttempt, TransBkey};
use crate::c;
use crate::errcode::{bch_errcode, ret_to_result_void, BchError};
use crate::fs::Fs;
use crate::snapshots::check_snapshots;
use crate::util::alloc::{flags::GFP_KERNEL, KVVec};
use crate::util::rcu;
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
    fn fs(&self) -> &Fs {
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

    /// By table entry, whatever state the nodes are in.
    pub fn same_tree(&self, id1: u32, id2: u32) -> bool {
        if id1 == id2 {
            return true;
        }
        let g = rcu::read_lock();
        let t = Table::get(self, &g);
        match (t.entry(id1), t.entry(id2)) {
            (Some(s1), Some(s2)) => s1.tree == s2.tree,
            _                    => false,
        }
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

    /// @id is a live node: as bch2_snapshot_exists().
    pub fn exists(&self, id: u32) -> bool {
        self.id_state(id) == c::snapshot_id_state::SNAPSHOT_ID_live
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
        // A parent at the maximum is damaged, and check_depth()'s to repair
        s.depth.saturating_add(1)
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
    pub fn table_find_edge(&self, s: &c::Snapshot, id: u32, parent: bool) -> Option<u32> {
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

    /// What's accounted to snapshot @id.
    ///
    /// In-mem: bch2_accounting_is_mem() covers everything but
    /// BCH_DISK_ACCOUNTING_inum, so the counters are current as deltas are
    /// applied. No btree read, no transaction, and no write buffer flush to
    /// make the values trustworthy.
    ///
    /// The key count catches metadata-only stranding (dirents, xattrs, empty
    /// inodes) that the sectors counter can't see. It's only trusted once
    /// check_allocations has rebuilt it (scheduled by the
    /// per_dev_fragmentation_lru upgrade); before that it reads as zero, and
    /// only sectors count.
    pub fn accounting(&self, id: u32) -> SnapshotAccounting {
        let fs = self.fs();
        let trust_keys = fs.version_upgrade_complete().0 >=
            c::bcachefs_metadata_version::bcachefs_metadata_version_per_dev_fragmentation_lru.0;

        let mut btrees = crate::btrees_in(crate::BTREE_HAS_SNAPSHOTS_MASK);
        SnapshotAccounting(core::array::from_fn(|_| {
            let btree = btrees.next().expect("NR_SNAPSHOT_BTREES is the mask's popcount");

            let mut v = [0u64; 3];
            crate::accounting::mem_read(fs, &c::disk_accounting_pos::from_arm(
                    c::bch_acct_snapshot { id, btree: btree.0 }), &mut v);

            BtreeAccounting {
                btree,
                keys:      if trust_keys { v[0] } else { 0 },
                key_bytes: if trust_keys { v[1] } else { 0 },
                sectors:   v[2],
            }
        }))
    }
}

const NR_SNAPSHOT_BTREES: usize = crate::BTREE_HAS_SNAPSHOTS_MASK.count_ones() as usize;

#[derive(Clone, Copy)]
struct BtreeAccounting {
    btree:     c::btree_id,
    keys:      u64,
    key_bytes: u64,
    sectors:   u64,
}

impl BtreeAccounting {
    fn is_empty(&self) -> bool {
        self.keys == 0 && self.sectors == 0
    }
}

/// Keys and sectors accounted to a snapshot, per snapshotted btree. Displays
/// as the per-btree breakdown, a line for each btree with anything in it.
pub struct SnapshotAccounting([BtreeAccounting; NR_SNAPSHOT_BTREES]);

impl SnapshotAccounting {
    pub fn keys(&self) -> u64    { self.0.iter().map(|b| b.keys).sum() }
    pub fn sectors(&self) -> u64 { self.0.iter().map(|b| b.sectors).sum() }

    pub fn is_empty(&self) -> bool {
        self.0.iter().all(BtreeAccounting::is_empty)
    }

    /// A bit per btree with anything accounted.
    pub fn btrees(&self) -> u64 {
        self.0.iter().filter(|b| !b.is_empty()).fold(0, |m, b| m | 1u64 << b.btree.0)
    }
}

impl fmt::Display for SnapshotAccounting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.0.iter().filter(|b| !b.is_empty()) {
            write!(f, "\n  {}: {} keys ({} bytes), {} sectors", b.btree, b.keys, b.key_bytes, b.sectors)?;
        }
        Ok(())
    }
}

/// Whether @id is @ancestor or one of its descendants.
pub fn is_ancestor(t: &TransAttempt<'_, '_>, id: u32, ancestor: u32) -> bool {
    debug_assert!(id != 0 && ancestor != 0);
    id == ancestor || unsafe { c::__bch2_snapshot_is_ancestor(t.raw(), id, ancestor) }
}

/// If repairs asked for it.
pub fn table_rebuild_if_needed(trans: &BtreeTrans<'_>) -> Result<(), BchError> {
    if !unsafe { (*trans.fs().as_ptr()).snapshots.need_table_rebuild } {
        return Ok(());
    }
    ret_to_result_void(unsafe { c::bch2_snapshot_table_rebuild(trans.raw()) })
}

/// The snapshots that have overwritten the key at @pos in @btree: on each
/// branch, the one closest to @pos.snapshot.
pub fn overwrites(t: &TransAttempt<'_, '_>, btree: c::btree_id, pos: c::bpos)
    -> Result<KVVec<u32>, BchError>
{
    let mut ids = KVVec::new();

    if !t.fs().snapshots.has_children(pos.snapshot) {
        return Ok(ids);
    }

    // From the version before @pos's - in the snapshot with the next lower
    // ID; @pos.snapshot has children, so it isn't 0 - down
    let mut iter = BtreeIter::new(t, btree, spos(pos.inode, pos.offset, pos.snapshot - 1),
                                  BtreeIterFlags::ALL_SNAPSHOTS);
    iter.for_each_reverse_norestart(t, spos(pos.inode, pos.offset, 0), |_, k| {
        let id = k.k.p.snapshot;
        if is_ancestor(t, id, pos.snapshot) &&
           !ids.iter().any(|&i| is_ancestor(t, id, i)) {
            ids.push(id, GFP_KERNEL)?;
        }
        Ok(())
    })?;
    Ok(ids)
}

pub fn tree_lookup(t: &TransAttempt<'_, '_>, id: u32) -> Result<c::SnapshotTree, BchError> {
    t.bkey_get_val::<c::SnapshotTree>(c::btree_id::snapshot_trees, pos(0, id as u64),
                                      BtreeIterFlags::empty())
        .map_err(|e| if e.matches(c::ENOENT) {
            t.fs().err(bch_errcode::BCH_ERR_ENOENT_snapshot_tree)
        } else {
            e
        })
}

/// At an unused id, queued for the commit, for the caller to fill in.
pub fn tree_create<'a, 't>(t: &TransAttempt<'a, 't>)
    -> Result<TransBkey<'a, 't, c::SnapshotTree>, BchError>
{
    // SAFETY: bch2_bkey_alloc(..., snapshot_tree): a whole snapshot tree key
    unsafe {
        let k = c::__bch2_snapshot_tree_create(t.raw());
        Ok(TransBkey::from_raw(t, k as *mut c::bkey_i)?.typed())
    }
}

/// Whether @k is in a snapshot that doesn't exist - reported, and @k deleted
/// or its snapshot reconstructed - for the caller to skip it.
pub fn check_key_has_snapshot(
    t:    &TransAttempt<'_, '_>,
    iter: &BtreeIter<'_>,
    k:    BkeySC<'_>,
) -> Result<bool, BchError> {
    if t.fs().snapshots.id_state(k.k.p.snapshot) == c::snapshot_id_state::SNAPSHOT_ID_live {
        return Ok(false);
    }
    // SAFETY: BtreeIter holds its iterator in an UnsafeCell, and nothing else
    // has it for the call
    let iter = unsafe { &mut *iter.raw() };
    check_snapshots::check_key_has_snapshot(t, Some(iter), k)
}

/// ENOENT if there's none.
pub fn lookup(t: &TransAttempt<'_, '_>, id: u32) -> Result<c::Snapshot, BchError> {
    t.bkey_get_val::<c::Snapshot>(c::btree_id::snapshots, pos(0, id as u64),
                                  BtreeIterFlags::empty())
}

/// The snapshot keys of the tree @start is in, for an error message: as
/// bch2_snapshot_tree_keys_to_text().
#[cold]
pub fn tree_keys_to_text(out: &mut Printbuf, t: &TransAttempt<'_, '_>, start: u32)
    -> Result<(), BchError>
{
    let ret = unsafe { c::bch2_snapshot_tree_keys_to_text(out.as_raw(), t.raw(), start) };
    t.result(ret)
}

/// ENOENT if there's none.
pub fn lookup_key(t: &TransAttempt<'_, '_>, id: u32) -> Result<BkeyI<c::Snapshot>, BchError> {
    t.bkey_get_key::<c::Snapshot>(c::btree_id::snapshots, pos(0, id as u64),
                                  BtreeIterFlags::empty())
}

/// Zero padded past the fields an older key lacks.
pub fn val(k: BkeySC<'_>) -> Option<c::Snapshot> {
    c::Snapshot::val_copy_pad(k)
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

/// Any snapshot node, in place or whole - the struct reaches these too.
impl c::Snapshot<Validated> {
    pub fn parent(&self) -> u32       { self.parent.get() }
    pub fn children(&self) -> [u32; 2] { self.children.map(|c| c.get()) }
    pub fn subvol(&self) -> u32       { self.subvol.get() }
    pub fn tree(&self) -> u32         { self.tree.get() }

    /// None if the field is 0 - the key predates it - or damaged.
    pub fn state_field(&self) -> Option<c::bch_snapshot_state> {
        SNAPSHOT_STATES.into_iter().find(|&s| s.0 == self.state().get())
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
    pub fn state_decoded(&self) -> Option<c::bch_snapshot_state> {
        if self.state() == 0 {
            Some(self.state_from_flags())
        } else {
            self.state_field()
        }
    }
}

/// The struct: a field an older key lacks, to set.
impl c::Snapshot<Whole> {
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

impl fmt::Display for c::Snapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        crate::printbuf_to_formatter(f, |out| unsafe { c::bch2_snapshot_to_text(out, self) })
    }
}
