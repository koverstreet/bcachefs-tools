// SPDX-License-Identifier: GPL-2.0

//! What Rust calls of init/fs.h - C gets it as prototypes, in init/fs_gen.h,
//! which is generated from this file: see fs/types/lib.rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use core::ffi::c_int;
use cstruct_macros::c_extern;

c_extern! {
    pub fn bch2_sbs_filter_dead(sbs: *mut c::bch_sb_handles, opts: *mut c::bch_opts,
                                out: *mut c::printbuf) -> c_int;
    pub fn bch2_fs_emergency_read_only(c: *mut c::bch_fs, out: *mut c::printbuf) -> bool;
    pub fn bch2_fs_read_only(c: *mut c::bch_fs);
    pub fn bch2_fs_start(c: *mut c::bch_fs) -> c_int;
    pub fn bch2_fs_exit(c: *mut c::bch_fs) -> c_int;
    pub fn bch2_fs_open(devices: *mut c::darray_const_str, opts: *mut c::bch_opts,
                        user_key: *const c::bch_key) -> *mut c::bch_fs;
}
