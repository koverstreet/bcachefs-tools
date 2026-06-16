// SPDX-License-Identifier: GPL-2.0

use core::ffi::{c_void, CStr};

use crate::btree::iter::BtreeTrans;
use crate::c;
use crate::errcode::{ret_to_result_void as ret_to_result, BchError};

pub fn set(
    trans: &BtreeTrans,
    inum:  c::subvol_inum,
    inode: &mut c::bch_inode_unpacked,
    name:  &CStr,
    val:   &[u8],
    typ:   i32,
    flags: i32,
) -> Result<(), BchError> {
    ret_to_result(unsafe {
        c::bch2_xattr_set(
            trans.raw(),
            inum,
            inode,
            name.as_ptr(),
            val.as_ptr() as *const c_void,
            val.len(),
            typ,
            flags,
        )
    })
}
