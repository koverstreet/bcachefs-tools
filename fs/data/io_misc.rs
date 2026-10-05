// SPDX-License-Identifier: GPL-2.0

use crate::btree::iter::BtreeTrans;
use crate::c;
use crate::errcode::{ret_to_result_void as ret_to_result, BchError};
use crate::fs::Fs;

/// For fsck: delete the extents from @start to @end in @start's snapshot,
/// committing what the caller queued first. It runs its own commit loop, so
/// as a nested transaction it returns transaction_restart_nested if that
/// restarted: as bch2_fpunch_snapshot().
pub fn fpunch_snapshot(trans: &BtreeTrans<'_>, start: c::bpos, end: c::bpos) -> Result<(), BchError> {
    ret_to_result(unsafe { c::bch2_fpunch_snapshot(trans.raw(), start, end) })
}

pub fn fpunch(
    fs:            &Fs,
    inum:          c::subvol_inum,
    start:         u64,
    end:           u64,
    sectors_delta: &mut i64,
) -> Result<(), BchError> {
    ret_to_result(unsafe {
        c::bch2_fpunch(fs.raw, inum, start, end, sectors_delta)
    })
}
