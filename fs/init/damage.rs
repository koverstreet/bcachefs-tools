// SPDX-License-Identifier: GPL-2.0

//! Damage tracking (init/damage.h): what recovery found lost, recorded so it
//! can be attributed to the files it belonged to.

use crate::btree::bkey::BkeySC;
use crate::btree::iter::{TransAttempt, TransBkey};
use crate::c;
use crate::errcode::{ret_to_result_void, BchError};
use crate::fs::Fs;

/// Inode @inum in @snapshot is being deleted: delete its damage record, or
/// the whiteout a cleared one left, if there is one - as
/// bch2_damage_delete().
pub fn delete(t: &TransAttempt<'_, '_>, inum: u64, snapshot: u32) -> Result<(), BchError> {
    ret_to_result_void(unsafe { c::bch2_damage_delete(t.raw(), inum, snapshot) })
}

/// Damage keys @a and @b merged into one at @pos, in @t's memory: their
/// entries' sorted union, occurrence counts summed (saturating), last times
/// maxed - as bch2_damage_keys_merge(). For a dying snapshot's key collapsing
/// into its live child's.
pub fn keys_merge<'a, 't>(
    t:   &TransAttempt<'a, 't>,
    pos: c::bpos,
    a:   BkeySC<'_>,
    b:   BkeySC<'_>,
) -> Result<TransBkey<'a, 't>, BchError> {
    unsafe {
        TransBkey::from_raw(t, c::bch2_damage_keys_merge(t.raw(), pos, a.into(), b.into()))
    }
}

/// Record the extents lost with the btree nodes the extents btree lost, as
/// damage against the inodes in those ranges: as
/// bch2_damage_record_lost_extents(). Has to run before check_extents' walk:
/// to the walk, an inode that lost every extent it had looks sparse.
pub fn record_lost_extents(fs: &Fs) {
    unsafe { crate::c::bch2_damage_record_lost_extents(fs.as_ptr()) }
}
