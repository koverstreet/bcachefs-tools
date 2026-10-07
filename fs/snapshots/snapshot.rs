// SPDX-License-Identifier: GPL-2.0

//! Snapshots (snapshots/snapshot.h).
//!
//! The state field is read raw and matched against the codewords here, never
//! taken from C as a bch_snapshot_state: a damaged field holds a value that's
//! no variant of the Rust enum, and fsck has to be able to look at it.

use crate::btree::bkey::BkeySC;
use crate::btree::iter::{BtreeIter, BtreeTrans, TransAttempt, TransBkey};
use crate::c;
use crate::c::bch_snapshot_state::*;
use crate::errcode::{ret_to_result, ret_to_result_void, BchError};
use crate::fs::Fs;
use crate::util::Printbuf;
use core::fmt;

/// If @id is a redundant interior snapshot node - left by an interrupted
/// deletion, to be collapsed into a live descendant - the live snapshot it
/// collapses into: as bch2_snapshot_redundant_interior().
pub fn redundant_interior(fs: &Fs, id: u32) -> Option<u32> {
    match unsafe { c::bch2_snapshot_redundant_interior(fs.raw, id) } {
        0 => None,
        terminal => Some(terminal),
    }
}

/// Whether snapshot @id is @ancestor or one of its descendants: as
/// bch2_snapshot_is_ancestor().
pub fn is_ancestor(trans: &BtreeTrans<'_>, id: u32, ancestor: u32) -> bool {
    unsafe { c::bch2_snapshot_is_ancestor(trans.raw(), id, ancestor) }
}

/// is_ancestor() by walking parent pointers, for before the ancestor bitmaps
/// are built: as bch2_snapshot_is_ancestor_early().
pub fn is_ancestor_early(fs: &Fs, id: u32, ancestor: u32) -> bool {
    unsafe { c::bch2_snapshot_is_ancestor_early(fs.raw, id, ancestor) }
}

/// Whether snapshot @id has children: as bch2_snapshot_has_children().
pub fn has_children(fs: &Fs, id: u32) -> bool {
    unsafe { c::bch2_snapshot_has_children(fs.raw, id) }
}

/// Whether snapshot @id is a leaf: as bch2_snapshot_is_leaf(). An error if
/// there's no such snapshot.
pub fn is_leaf(fs: &Fs, id: u32) -> Result<bool, BchError> {
    Ok(ret_to_result(unsafe { c::bch2_snapshot_is_leaf(fs.raw, id) })? != 0)
}

/// The snapshot tree @id belongs to, 0 if there's no such snapshot: as
/// bch2_snapshot_tree().
pub fn tree(fs: &Fs, id: u32) -> u32 {
    unsafe { c::bch2_snapshot_tree(fs.raw, id) }
}

/// The root of @id's snapshot tree: as bch2_snapshot_root().
pub fn root(fs: &Fs, id: u32) -> u32 {
    unsafe { c::bch2_snapshot_root(fs.raw, id) }
}

/// What the in-memory snapshot table has for @id: as
/// bch2_snapshot_id_state().
pub fn id_state(fs: &Fs, id: u32) -> c::snapshot_id_state {
    unsafe { c::bch2_snapshot_id_state(fs.raw, id) }
}

/// The depth of a child of @parent - 0 for no parent: as
/// bch2_snapshot_depth(). @parent must be in the table.
pub fn depth(fs: &Fs, parent: u32) -> u32 {
    unsafe { c::bch2_snapshot_depth(fs.raw, parent) }
}

/// A random ancestor of @id, for a skiplist entry - @id itself for a root, 0
/// for 0: as bch2_snapshot_skiplist_get().
pub fn skiplist_get(fs: &Fs, id: u32) -> u32 {
    unsafe { c::bch2_snapshot_skiplist_get(fs.raw, id) }
}

/// The lowest numbered subvolume in @root's subtree: as
/// bch2_snapshot_oldest_subvol().
pub fn oldest_subvol(fs: &Fs, root: u32) -> Option<u32> {
    match unsafe { c::bch2_snapshot_oldest_subvol(fs.raw, root) } {
        0 => None,
        subvol => Some(subvol),
    }
}

/// The unique live node claiming an edge with @id - as its parent if
/// @parent, else as its child - other than those @s, @id's key, points at:
/// as bch2_snapshot_table_find_edge(). None if there's none, or more than one.
pub fn table_find_edge(fs: &Fs, s: &c::bch_snapshot, id: u32, parent: bool) -> Option<u32> {
    match unsafe { c::bch2_snapshot_table_find_edge(fs.raw, s, id, parent) } {
        0 => None,
        n => Some(n),
    }
}

/// Make room in the table for node @id, about to be created: as
/// bch2_snapshot_table_make_room().
pub fn table_make_room(fs: &Fs, id: u32) -> Result<(), BchError> {
    ret_to_result_void(unsafe { c::bch2_snapshot_table_make_room(fs.raw, id) })
}

/// Have the table rebuilt from the btree, after repairs it doesn't track:
/// see table_rebuild_if_needed().
pub fn set_need_table_rebuild(fs: &Fs) {
    unsafe { (*fs.raw).snapshots.need_table_rebuild = true }
}

/// Rebuild the in-memory table from the btree, if repairs asked for it: as
/// bch2_snapshot_table_rebuild().
pub fn table_rebuild_if_needed(trans: &BtreeTrans<'_>) -> Result<(), BchError> {
    if !unsafe { (*trans.fs().raw).snapshots.need_table_rebuild } {
        return Ok(());
    }
    ret_to_result_void(unsafe { c::bch2_snapshot_table_rebuild(trans.raw()) })
}

/// The live snapshot @id is, or collapses into if it's dead: as
/// bch2_snapshot_live_descendent(). None if there isn't one; an error if the
/// tree is damaged on the way.
pub fn live_descendent(fs: &Fs, id: u32) -> Result<Option<u32>, BchError> {
    let mut live = 0;
    ret_to_result(unsafe { c::bch2_snapshot_live_descendent(fs.raw, id, &mut live) })?;
    Ok((live != 0).then_some(live))
}

/// The keys and sectors accounted to snapshot @id, across the snapshotted
/// btrees - a per-btree breakdown to @breakdown: as
/// bch2_snapshot_accounting_totals().
pub fn accounting_totals(fs: &Fs, id: u32, breakdown: Option<&mut Printbuf>)
    -> Result<(u64, u64), BchError>
{
    let (mut keys, mut sectors) = (0, 0);
    let out = breakdown.map_or(core::ptr::null_mut(), |b| b.as_raw() as *mut _);
    ret_to_result_void(unsafe {
        c::bch2_snapshot_accounting_totals(fs.raw, id, &mut keys, &mut sectors,
                                           core::ptr::null_mut(), out)
    })?;
    Ok((keys, sectors))
}

/// Snapshot tree @id's key: as bch2_snapshot_tree_lookup().
pub fn tree_lookup(trans: &BtreeTrans<'_>, id: u32) -> Result<c::bch_snapshot_tree, BchError> {
    let mut t = c::bch_snapshot_tree::default();
    ret_to_result(unsafe { c::bch2_snapshot_tree_lookup(trans.raw(), id, &mut t) })?;
    Ok(t)
}

/// A new snapshot tree key, at an unused id, queued for the commit - for the
/// caller to fill in: as __bch2_snapshot_tree_create().
pub fn tree_create<'a, 't>(t: &TransAttempt<'a, 't>) -> Result<TransBkey<'a, 't>, BchError> {
    unsafe {
        let k = c::__bch2_snapshot_tree_create(t.raw());
        TransBkey::from_raw(t, k as *mut c::bkey_i)
    }
}

/// Whether @k is in a snapshot that doesn't exist - reported, and @k deleted
/// or its snapshot reconstructed - so the caller skips it: as
/// bch2_check_key_has_snapshot().
pub fn check_key_has_snapshot(
    trans: &BtreeTrans<'_>,
    iter:  &BtreeIter<'_>,
    k:     BkeySC<'_>,
) -> Result<bool, BchError> {
    let ret = unsafe { c::bch2_check_key_has_snapshot(trans.raw(), iter.raw(), k.to_raw()) };
    Ok(ret_to_result(ret)? > 0)
}

/// Snapshot node @id: as bch2_snapshot_lookup(). ENOENT if there's none.
pub fn lookup(trans: &BtreeTrans<'_>, id: u32) -> Result<c::bch_snapshot, BchError> {
    let mut s = c::bch_snapshot::default();
    ret_to_result(unsafe { c::bch2_snapshot_lookup(trans.raw(), id, &mut s) })?;
    Ok(s)
}

/// Snapshot node @id's key, for when the key itself is wanted - to print: as
/// bch2_snapshot_lookup_key(). ENOENT if there's none.
pub fn lookup_key(trans: &BtreeTrans<'_>, id: u32) -> Result<c::bkey_i_snapshot, BchError> {
    let mut k = c::bkey_i_snapshot::default();
    ret_to_result(unsafe { c::bch2_snapshot_lookup_key(trans.raw(), id, &mut k) })?;
    Ok(k)
}

/// @k's value if it's a snapshot node, zero padded past the fields an older
/// key lacks: as bkey_val_copy_pad() on a bkey_s_c_snapshot.
pub fn val(k: BkeySC<'_>) -> Option<c::bch_snapshot> {
    (k.k.type_ == c::bch_bkey_type::KEY_TYPE_snapshot.0 as u8)
        .then(|| unsafe { k.val_copy_pad() })
}

/// Put deleted node @u back in the tree, relinking it under its parent: as
/// bch2_snapshot_node_undelete().
pub fn node_undelete<'t>(t: &TransAttempt<'_, 't>, u: &mut TransBkey<'_, 't>)
    -> Result<(), BchError>
{
    let ret = unsafe { c::bch2_snapshot_node_undelete(t.raw(), u.as_ptr() as *mut c::bkey_i_snapshot) };
    t.result(ret)
}

/// Move @k, the key at @iter in a dead snapshot, to @live_child - or delete
/// it, for 0: as bch2_delete_dead_snapshot_key().
pub fn delete_dead_key<'a, 't>(
    t:          &TransAttempt<'a, 't>,
    iter:       &mut c::btree_iter,
    k:          BkeySC<'_>,
    live_child: u32,
) -> Result<(), BchError> {
    let ret = unsafe { c::bch2_delete_dead_snapshot_key(t.raw(), iter, k.to_raw(), live_child) };
    t.result(ret)
}

/// The state codeword nearest @v, and its Hamming distance from @v: as
/// bch2_snapshot_state_nearest().
pub fn state_nearest(v: u32) -> (c::bch_snapshot_state, u32) {
    let mut dist = 0;
    let s = unsafe { c::bch2_snapshot_state_nearest(v, &mut dist) };
    (s, dist)
}

impl c::bch_snapshot {
    pub fn parent(&self) -> u32       { u32::from_le(self.parent) }
    pub fn children(&self) -> [u32; 2] { self.children.map(u32::from_le) }
    pub fn subvol(&self) -> u32       { u32::from_le(self.subvol) }
    pub fn tree(&self) -> u32         { u32::from_le(self.tree) }
    pub fn depth(&self) -> u32        { u32::from_le(self.depth) }

    /// The raw state field.
    pub fn state_raw(&self) -> u32 {
        u32::from_le(self.state)
    }

    /// The state field, if it holds a state - not if it's 0, predating the
    /// field, or damaged: as bch2_snapshot_state() and
    /// bch2_snapshot_state_valid().
    pub fn state_field(&self) -> Option<c::bch_snapshot_state> {
        [SNAPSHOT_STATE_live, SNAPSHOT_STATE_will_delete,
         SNAPSHOT_STATE_no_keys, SNAPSHOT_STATE_deleted]
            .into_iter()
            .find(|&s| s as u32 == self.state_raw())
    }

    /// The state the legacy flag bits say: as
    /// bch2_snapshot_state_from_flags().
    pub fn state_from_flags(&self) -> c::bch_snapshot_state {
        unsafe { c::bch2_snapshot_state_from_flags(self) }
    }

    /// The node's state - read from its flags if it predates the state field:
    /// as bch2_snapshot_state_compat(). None if the field is damaged.
    pub fn state(&self) -> Option<c::bch_snapshot_state> {
        if self.state == 0 {
            Some(self.state_from_flags())
        } else {
            self.state_field()
        }
    }

    /// Set the node's state, and the legacy flags to match: as
    /// bch2_snapshot_state_set().
    pub fn set_state(&mut self, state: c::bch_snapshot_state) {
        unsafe { c::bch2_snapshot_state_set(self, state) }
    }
}

/// As bch2_snapshot_state_str().
impl fmt::Display for c::bch_snapshot_state {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = unsafe { core::ffi::CStr::from_ptr(c::bch2_snapshot_state_str(*self)) };
        f.write_str(s.to_str().unwrap_or("?"))
    }
}

/// As bch2_snapshot_to_text().
impl fmt::Display for c::bch_snapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        crate::printbuf_to_formatter(f, |out| unsafe { c::bch2_snapshot_to_text(out, self) })
    }
}
