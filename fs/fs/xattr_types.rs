// SPDX-License-Identifier: GPL-2.0

//! The data types of fs/xattr_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::{CStruct, c_extern, c_verbatim, rust_c_extern};
use crate::xattr::{bch2_xattr_to_text, bch2_xattr_validate};

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct xattr_search_key {
    pub type_: u8,
    pub name: c::qstr,
}
c_default!(xattr_search_key);

c_verbatim!(r#"
struct dentry;
struct xattr_handler;
"#);

c_verbatim!(r#"
struct bch_hash_info;
struct bch_inode_info;
"#);

#[cfg(not(NO_BCACHEFS_FS))]
c_verbatim!(r#"
/* The handlers - fs/xattr.rs - for xattr.c's tables: */
struct mnt_idmap;

struct inode;
"#);

// bkey_ops' methods, Rust's: defined with C's signatures - see rust_c_extern!.
rust_c_extern! {
    pub fn bch2_xattr_validate(arg1: *mut c::bch_fs, arg2: c::bkey_s_c, arg3: *const c::bkey_validate_context) -> core::ffi::c_int;
    pub fn bch2_xattr_to_text(arg1: *mut c::printbuf, arg2: *mut c::bch_fs, arg3: c::bkey_s_c);
}

// What Rust calls of fs/xattr.h: C gets these as prototypes, in fs/xattr_gen.h
// with the types - see c_extern! in types/cstruct.rs.
c_extern! {
    pub fn bch2_xattr_get_trans(arg1: *mut c::btree_trans, arg2: *const c::bch_inode_unpacked, arg3: c::subvol_inum, arg4: core::ffi::c_int, arg5: *const crate::util::ffi::c_char, arg6: *mut core::ffi::c_void, arg7: usize) -> core::ffi::c_int;
    pub fn __bch2_xattr_set(arg1: *mut c::btree_trans, arg2: c::subvol_inum, arg3: *const c::bch_hash_info, arg4: *const crate::util::ffi::c_char, arg5: *const core::ffi::c_void, arg6: usize, arg7: core::ffi::c_int, arg8: core::ffi::c_int) -> core::ffi::c_int;
    pub fn bch2_xattr_set(arg1: *mut c::btree_trans, arg2: c::subvol_inum, arg3: *mut c::bch_inode_unpacked, arg4: *const crate::util::ffi::c_char, arg5: *const core::ffi::c_void, arg6: usize, arg7: core::ffi::c_int, arg8: core::ffi::c_int) -> core::ffi::c_int;
}

// For listxattr, as xattr.h has it: kernel only.
#[cfg(not(NO_BCACHEFS_FS))]
c_extern! {
    pub fn bch2_xattr_list_prefix(type_: core::ffi::c_uint, dentry: *mut c::dentry) -> *const crate::util::ffi::c_char;
}
