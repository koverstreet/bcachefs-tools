// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use crate::util::ffi::c_char;
use core::ffi::{c_int, c_void};
#[cfg(not(NO_BCACHEFS_FS))]
use core::ffi::c_uint;
use cstruct_macros::{CStruct, c_extern, rust_c_extern};
use crate::xattr::{bch2_xattr_to_text, bch2_xattr_validate};

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct xattr_search_key {
    pub type_: u8,
    pub name: c::qstr,
}
c_default!(xattr_search_key);

// bkey_ops' methods, Rust's: defined with C's signatures - see rust_c_extern!.
rust_c_extern! {
    pub fn bch2_xattr_validate(arg1: *mut c::bch_fs, arg2: c::bkey_s_c, arg3: *const c::bkey_validate_context) -> c_int;
    pub fn bch2_xattr_to_text(arg1: *mut c::printbuf, arg2: *mut c::bch_fs, arg3: c::bkey_s_c);
}

c_extern! {
    pub fn bch2_xattr_get_trans(arg1: *mut c::btree_trans, arg2: *const c::bch_inode_unpacked, arg3: c::subvol_inum, arg4: c_int, arg5: *const c_char, arg6: *mut c_void, arg7: usize) -> c_int;
    pub fn __bch2_xattr_set(arg1: *mut c::btree_trans, arg2: c::subvol_inum, arg3: *const c::bch_hash_info, arg4: *const c_char, arg5: *const c_void, arg6: usize, arg7: c_int, arg8: c_int) -> c_int;
    pub fn bch2_xattr_set(arg1: *mut c::btree_trans, arg2: c::subvol_inum, arg3: *mut c::bch_inode_unpacked, arg4: *const c_char, arg5: *const c_void, arg6: usize, arg7: c_int, arg8: c_int) -> c_int;
}

// For listxattr, as xattr.h has it: kernel only.
#[cfg(not(NO_BCACHEFS_FS))]
c_extern! {
    pub fn bch2_xattr_list_prefix(type_: c_uint, dentry: *mut c::dentry) -> *const c_char;
}
