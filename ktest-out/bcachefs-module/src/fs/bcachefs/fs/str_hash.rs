// SPDX-License-Identifier: GPL-2.0

use crate::c;
use crate::errcode::{ret_to_result_void as ret_to_result, BchError};
use crate::fs::Fs;

pub fn hash_info_init(
    fs:    &Fs,
    inode: &c::bch_inode_unpacked,
) -> Result<c::bch_hash_info, BchError> {
    let mut hash_info: c::bch_hash_info = Default::default();
    ret_to_result(unsafe {
        c::bch2_hash_info_init(fs.raw, inode, &mut hash_info)
    })?;
    Ok(hash_info)
}
