// SPDX-License-Identifier: GPL-2.0

//! What Rust calls of init/fs.h - C gets it as prototypes, in init/fs_gen.h,
//! which is generated from this file: see fs/types/lib.rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use cstruct_macros::c_extern;

// What Rust calls of init/fs.h: C gets these as prototypes, in init/fs_gen.h
// with the types - see c_extern! in types/cstruct.rs.
c_extern! {
    pub fn bch2_sbs_filter_dead(arg1: *mut c::bch_sb_handles, arg2: *mut c::bch_opts, arg3: *mut c::printbuf) -> core::ffi::c_int;
    pub fn bch2_fs_emergency_read_only(arg1: *mut c::bch_fs, arg2: *mut c::printbuf) -> bool;
    pub fn bch2_fs_read_only(arg1: *mut c::bch_fs);
    pub fn bch2_fs_read_write(arg1: *mut c::bch_fs) -> core::ffi::c_int;
    pub fn bch2_fs_start(arg1: *mut c::bch_fs) -> core::ffi::c_int;
    pub fn bch2_fs_exit(arg1: *mut c::bch_fs) -> core::ffi::c_int;
    pub fn bch2_fs_open(arg1: *mut c::darray_const_str, arg2: *mut c::bch_opts, arg3: *const c::bch_key) -> *mut c::bch_fs;
}
