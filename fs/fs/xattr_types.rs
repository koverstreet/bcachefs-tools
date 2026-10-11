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
    pub fn bch2_xattr_validate(c: *mut c::bch_fs, k: c::bkey_s_c, from: *const c::bkey_validate_context) -> c_int;
    pub fn bch2_xattr_to_text(out: *mut c::printbuf, c: *mut c::bch_fs, k: c::bkey_s_c);
}

c_extern! {
    pub fn bch2_xattr_get_trans(trans: *mut c::btree_trans, inode: *const c::bch_inode_unpacked, inum: c::subvol_inum, type_: c_int, name: *const c_char, buffer: *mut c_void, size: usize) -> c_int;
    pub fn __bch2_xattr_set(trans: *mut c::btree_trans, inum: c::subvol_inum, hash_info: *const c::bch_hash_info, name: *const c_char, value: *const c_void, size: usize, type_: c_int, flags: c_int) -> c_int;
    pub fn bch2_xattr_set(trans: *mut c::btree_trans, inum: c::subvol_inum, inode: *mut c::bch_inode_unpacked, name: *const c_char, value: *const c_void, size: usize, type_: c_int, flags: c_int) -> c_int;
}

// For listxattr, as xattr.h has it: kernel only.
#[cfg(not(NO_BCACHEFS_FS))]
c_extern! {
    pub fn bch2_xattr_list_prefix(type_: c_uint, dentry: *mut c::dentry) -> *const c_char;
}
