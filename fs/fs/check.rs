// SPDX-License-Identifier: GPL-2.0

//! fsck helpers shared by the passes (fs/check.h).

use crate::btree::iter::BtreeTrans;
use crate::c;
use crate::errcode::{ret_to_result_void as ret_to_result, BchError};
use crate::fs::Fs;

/// The snapshot IDs of the keys seen so far at one position, for deciding
/// visibility while walking a btree in key order with all snapshots: a key
/// in an ancestor snapshot is hidden from a descendant that overwrote it.
///
/// The ID list is a darray C allocates, so C frees it.
pub struct SnapshotsSeen(c::snapshots_seen);

impl SnapshotsSeen {
    pub fn new() -> Self {
        SnapshotsSeen(unsafe { c::snapshots_seen_init() })
    }

    /// Record a key at @pos, starting the list over if @pos is a new
    /// position: as bch2_snapshots_seen_update().
    pub fn update(&mut self, fs: &Fs, btree: c::btree_id, pos: c::bpos) -> Result<(), BchError> {
        ret_to_result(unsafe { c::bch2_snapshots_seen_update(fs.raw, &mut self.0, btree, pos) })
    }

    /// Whether a reference from a key in @src points at something visible in
    /// some snapshot as a key in @dst: as bch2_ref_visible(). Assumes the @src
    /// keys are being visited in key order, with this list recording them.
    pub fn ref_visible(&mut self, trans: &BtreeTrans<'_>, src: u32, dst: u32) -> bool {
        unsafe { c::bch2_ref_visible(trans.raw(), &mut self.0, src, dst) }
    }
}

impl Default for SnapshotsSeen {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for SnapshotsSeen {
    fn drop(&mut self) {
        unsafe { c::snapshots_seen_exit(&mut self.0) }
    }
}
