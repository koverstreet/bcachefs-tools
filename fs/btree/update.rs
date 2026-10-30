// SPDX-License-Identifier: GPL-2.0

//! Btree updates - btree/update.c's, as they're converted.

use crate::c;
use crate::fs::Fs;
use crate::util::ffi::Opaque;
use cstruct_macros::c_export;

/// The whiteout that deletes @k in @btree. A KEY_TYPE_extent_whiteout says
/// there's no extent there: key start positions, extent_whiteouts included -
/// KEY_TYPE_whiteouts aren't - only go up. It's a deletion's in a leaf
/// snapshot of an extents btree, once the filesystem has the feature, which
/// this asks for; a snapshot the table doesn't have is a leaf, as C took it.
/// Any other deletion's is KEY_TYPE_whiteout.
pub fn extent_whiteout_type(fs: &Fs, btree: c::btree_id, k: &c::bkey) -> c::bch_bkey_type {
    if btree.is_extents_snapshots() &&
       k.is_deleted() &&
       fs.snapshots.is_leaf(k.p.snapshot) != Ok(false) &&
       fs.request_incompat_feature(c::bcachefs_metadata_version::extent_snapshot_whiteouts).is_ok() {
        c::bch_bkey_type::KEY_TYPE_extent_whiteout
    } else {
        c::bch_bkey_type::KEY_TYPE_whiteout
    }
}

#[c_export]
pub fn bch2_extent_whiteout_type(c: &Opaque<c::bch_fs>, btree: c::btree_id, k: &c::bkey) -> c::bch_bkey_type {
    extent_whiteout_type(Fs::from_c(c), btree, k)
}
