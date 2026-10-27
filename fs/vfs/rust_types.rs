// SPDX-License-Identifier: GPL-2.0

//! The VFS, for Rust: shims out of line in vfs/fs.c, as locking/locking.c's are.
//! C gets the prototypes, in vfs/rust_gen.h. Kernel only, as the VFS is.
//!
//! What's here is what Rust calls of fs.c: bch2_set_projid(), a static
//! inline, through a shim.

#![allow(non_camel_case_types)]
// The VFS's: kernel only, as C's #ifndef NO_BCACHEFS_FS
#![cfg(not(NO_BCACHEFS_FS))]

use crate::cstructs::c;
use core::ffi::c_int;
use cstruct_macros::c_extern;

c_extern! {
    pub fn rust_ei_set_projid(c: *mut c::bch_fs, ei: *mut c::bch_inode_info, projid: u32) -> c_int;
}
