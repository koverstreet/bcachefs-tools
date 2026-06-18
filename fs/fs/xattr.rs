// SPDX-License-Identifier: GPL-2.0

use core::ffi::{c_void, CStr};

use crate::btree::iter::{TransAttempt, TransError};
use crate::c;

pub fn set<'a, 't>(
    t:     TransAttempt<'a, 't>,
    inum:  c::subvol_inum,
    inode: &mut c::bch_inode_unpacked,
    name:  &CStr,
    val:   &[u8],
    typ:   i32,
    flags: i32,
) -> Result<TransAttempt<'a, 't>, TransError> {
    let ret = unsafe {
        c::bch2_xattr_set(
            t.raw(),
            inum,
            inode,
            name.as_ptr(),
            val.as_ptr() as *const c_void,
            val.len(),
            typ,
            flags,
        )
    };
    t.result(ret)
}
