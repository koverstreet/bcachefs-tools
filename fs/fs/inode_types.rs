// SPDX-License-Identifier: GPL-2.0

//! What C calls of fs/inode.h's Rust: bkey_ops' methods, defined with C's
//! signatures - see rust_c_extern!.

use cstruct_macros::rust_c_extern;
use crate::c;
use crate::inode::{
    bch2_inode_alloc_cursor_to_text, bch2_inode_alloc_cursor_validate,
    bch2_inode_generation_to_text, bch2_inode_generation_validate,
    bch2_inode_to_text, bch2_inode_v2_validate, bch2_inode_v3_validate,
    bch2_inode_validate, bch2_trigger_inode,
};

rust_c_extern! {
    pub fn bch2_inode_validate(arg1: *mut c::bch_fs, arg2: c::bkey_s_c, arg3: *const c::bkey_validate_context) -> core::ffi::c_int;
    pub fn bch2_inode_v2_validate(arg1: *mut c::bch_fs, arg2: c::bkey_s_c, arg3: *const c::bkey_validate_context) -> core::ffi::c_int;
    pub fn bch2_inode_v3_validate(arg1: *mut c::bch_fs, arg2: c::bkey_s_c, arg3: *const c::bkey_validate_context) -> core::ffi::c_int;
    pub fn bch2_inode_to_text(arg1: *mut c::printbuf, arg2: *mut c::bch_fs, arg3: c::bkey_s_c);
    pub fn bch2_trigger_inode(arg1: *mut c::btree_trans, arg2: c::btree_trigger_op) -> core::ffi::c_int;
    pub fn bch2_inode_generation_validate(arg1: *mut c::bch_fs, arg2: c::bkey_s_c, arg3: *const c::bkey_validate_context) -> core::ffi::c_int;
    pub fn bch2_inode_generation_to_text(arg1: *mut c::printbuf, arg2: *mut c::bch_fs, arg3: c::bkey_s_c);
    pub fn bch2_inode_alloc_cursor_validate(arg1: *mut c::bch_fs, arg2: c::bkey_s_c, arg3: *const c::bkey_validate_context) -> core::ffi::c_int;
    pub fn bch2_inode_alloc_cursor_to_text(arg1: *mut c::printbuf, arg2: *mut c::bch_fs, arg3: c::bkey_s_c);
}
