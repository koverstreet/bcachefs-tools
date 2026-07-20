// SPDX-License-Identifier: GPL-2.0

use crate::c;
use crate::errcode::{ret_to_result_void as ret_to_result, BchError};
use crate::fs::Fs;

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
