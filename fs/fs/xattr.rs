// SPDX-License-Identifier: GPL-2.0

use core::ffi::{c_void, CStr};

use crate::btree::iter::TransAttempt;
use crate::c;
use crate::errcode::BchError;
use crate::str_hash::HashTable;

/// The xattrs btree, as a hash table.
pub struct Xattrs;

impl HashTable for Xattrs {
    type Key = c::xattr_search_key;

    fn desc() -> &'static c::bch_hash_desc {
        unsafe { &c::bch2_xattr_hash_desc }
    }
}

/// What an xattr lookup searches for, a type and name: C's X_SEARCH().
pub fn search_key(type_: u32, name: &[u8]) -> c::xattr_search_key {
    c::xattr_search_key { type_: type_ as u8, name: crate::dirent::qstr(name) }
}

pub fn set(
    t:     &TransAttempt<'_, '_>,
    inum:  c::subvol_inum,
    inode: &mut c::bch_inode_unpacked,
    name:  &CStr,
    val:   &[u8],
    typ:   i32,
    flags: i32,
) -> Result<(), BchError> {
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
