// SPDX-License-Identifier: GPL-2.0

//! The VFS, for Rust: shims out of line in vfs/fs.c, as locking/locking.c's are.
//! C gets the prototypes, in vfs/rust_gen.h. Kernel only, as the VFS is.
//!
//! What's here is what Rust can't reach itself: fs.c's statics -
//! bch2_set_projid() - and the dentry's internals.

#![allow(non_camel_case_types)]
// The VFS's: kernel only, as C's #ifndef NO_BCACHEFS_FS
#![cfg(not(NO_BCACHEFS_FS))]

use crate::cstructs::c;
use core::ffi::{c_int, c_uint};
use cstruct_macros::c_extern;

c_extern! {
    pub fn rust_ei_set_projid(c: *mut c::bch_fs, ei: *mut c::bch_inode_info, projid: u32) -> c_int;

    /* dentries: */
    pub fn rust_dentry_ei(dentry: *mut c::dentry) -> *mut c::bch_inode_info;
    pub fn rust_dentry_parent_inode_opt(dentry: *mut c::dentry, opt: c_uint, v: *mut u64) -> bool;
    pub fn rust_dir_casefold_changed(dentry: *mut c::dentry);
}
