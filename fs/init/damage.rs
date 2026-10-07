// SPDX-License-Identifier: GPL-2.0

//! Damage tracking (init/damage.h): what recovery found lost, recorded so it
//! can be attributed to the files it belonged to.

use crate::btree::iter::BtreeTrans;
use crate::c;
use crate::errcode::{ret_to_result_void, BchError};
use crate::fs::Fs;

/// Inode @inum in @snapshot is being deleted: delete its damage record, or
/// the whiteout a cleared one left, if there is one - as
/// bch2_damage_delete().
pub fn delete(trans: &BtreeTrans<'_>, inum: u64, snapshot: u32) -> Result<(), BchError> {
    ret_to_result_void(unsafe { c::bch2_damage_delete(trans.raw(), inum, snapshot) })
}

/// Record the extents lost with the btree nodes the extents btree lost, as
/// damage against the inodes in those ranges: as
/// bch2_damage_record_lost_extents(). Has to run before check_extents' walk:
/// to the walk, an inode that lost every extent it had looks sparse.
pub fn record_lost_extents(fs: &Fs) {
    unsafe { crate::c::bch2_damage_record_lost_extents(fs.raw) }
}
