// SPDX-License-Identifier: GPL-2.0

//! Snapshot deletion (delete.c), in Rust as it's converted: so far, its
//! module parameter.

use cstruct_macros::c_export;

crate::module_param! {
    /// Stop the key sweep after the extents/dirents/xattrs/damage pass and
    /// before the inodes are deleted, so a test can construct the state an
    /// interrupted deletion actually leaves: content gone for the dying
    /// snapshots, the inodes that indexed it still there, nodes still live.
    ///
    /// That boundary and not another one: the inodes btree is the index the
    /// sweep scans through, so it is the last point at which anything the
    /// sweep missed is still findable.
    pub(crate) static snapshot_delete_bail_before_inodes: bool = false;
    #[cfg(CONFIG_BCACHEFS_DEBUG)]
    perm = 0o644, desc = "Cancel snapshot deletion before the inode keys are deleted";
}

/// For delete.c, while it's C.
#[c_export]
pub fn bch2_snapshot_delete_bail_before_inodes() -> bool {
    snapshot_delete_bail_before_inodes.get()
}
