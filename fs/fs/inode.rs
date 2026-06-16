// SPDX-License-Identifier: GPL-2.0

use crate::c;
use crate::errcode::{ret_to_result_void as ret_to_result, BchError};
use crate::fs::Fs;

pub fn find_by_inum(
    fs:   &Fs,
    inum: c::subvol_inum,
) -> Result<c::bch_inode_unpacked, BchError> {
    let mut inode: c::bch_inode_unpacked = Default::default();
    ret_to_result(unsafe {
        c::bch2_inode_find_by_inum(fs.raw, inum, &mut inode)
    })?;
    Ok(inode)
}

pub fn init_early(fs: &Fs, inode: &mut c::bch_inode_unpacked) {
    unsafe { c::bch2_inode_init_early(fs.raw, inode) };
}

pub fn rm(fs: &Fs, inum: c::subvol_inum) -> Result<(), BchError> {
    ret_to_result(unsafe { c::bch2_inode_rm(fs.raw, inum) })
}
