// SPDX-License-Identifier: GPL-2.0

//! What Rust calls of fs/check.h - C gets it as prototypes, in fs/check_gen.h,
//! which is generated from this file: see fs/types/lib.rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use core::ffi::c_int;
use cstruct_macros::c_extern;

c_extern! {
    pub fn bch2_check_inodes(arg1: *mut c::bch_fs) -> c_int;
    pub fn bch2_check_extents(arg1: *mut c::bch_fs) -> c_int;
    pub fn bch2_check_indirect_extents(arg1: *mut c::bch_fs) -> c_int;
    pub fn bch2_check_dirents(arg1: *mut c::bch_fs) -> c_int;
    pub fn bch2_check_xattrs(arg1: *mut c::bch_fs) -> c_int;
    pub fn bch2_check_root(arg1: *mut c::bch_fs) -> c_int;
    pub fn bch2_check_subvolume_structure(arg1: *mut c::bch_fs) -> c_int;
    pub fn bch2_check_unreachable_inodes(arg1: *mut c::bch_fs) -> c_int;
    pub fn bch2_check_directory_structure(arg1: *mut c::bch_fs) -> c_int;
    pub fn bch2_check_nlinks(arg1: *mut c::bch_fs) -> c_int;
    pub fn bch2_fix_reflink_p(arg1: *mut c::bch_fs) -> c_int;
    pub fn bch2_fs_fsck_errcode(arg1: *mut c::bch_fs, arg2: *mut c::printbuf) -> c_int;
}
