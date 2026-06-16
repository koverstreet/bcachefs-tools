// SPDX-License-Identifier: GPL-2.0

use crate::c;
use crate::errcode::{self, ret_to_result_void as ret_to_result, BchError};
use crate::fs::Fs;

pub fn lookup(
    fs:        &Fs,
    dir_inum:  c::subvol_inum,
    hash_info: &c::bch_hash_info,
    name:      &c::qstr,
) -> Result<c::subvol_inum, BchError> {
    let mut inum: c::subvol_inum = Default::default();
    let ret = unsafe {
        c::bch2_dirent_lookup(fs.raw, dir_inum, hash_info, name, &mut inum)
    };
    errcode::ret_to_result(ret as i32)?;
    Ok(inum)
}

pub fn readdir(
    fs:        &Fs,
    dir_inum:  c::subvol_inum,
    hash_info: &mut c::bch_hash_info,
    ctx:       &mut c::dir_context,
) -> Result<(), BchError> {
    ret_to_result(unsafe {
        c::bch2_readdir(fs.raw, dir_inum, hash_info, ctx)
    })
}
