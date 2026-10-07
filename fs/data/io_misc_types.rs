// SPDX-License-Identifier: GPL-2.0

//! What Rust calls of data/io_misc.h - C gets it as prototypes, in data/io_misc_gen.h,
//! which is generated from this file: see fs/types/lib.rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use cstruct_macros::c_extern;

// What Rust calls of data/io_misc.h: C gets these as prototypes, in data/io_misc_gen.h
// with the types - see c_extern! in types/cstruct.rs.
c_extern! {
    pub fn bch2_fpunch_snapshot(arg1: *mut c::btree_trans, arg2: c::bpos, arg3: c::bpos) -> core::ffi::c_int;
    pub fn bch2_fpunch(c: *mut c::bch_fs, arg1: c::subvol_inum, arg2: u64, arg3: u64, arg4: *mut i64) -> core::ffi::c_int;
    pub fn bch2_truncate(arg1: *mut c::bch_fs, arg2: c::subvol_inum, arg3: u64, arg4: *mut u64) -> core::ffi::c_int;
}
