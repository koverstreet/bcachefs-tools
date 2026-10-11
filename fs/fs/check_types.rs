// SPDX-License-Identifier: GPL-2.0

//! What Rust calls of fs/check.h - C gets it as prototypes, in fs/check_gen.h,
//! which is generated from this file: see fs/types/lib.rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use core::ffi::c_int;
use cstruct_macros::c_extern;

c_extern! {
    pub fn bch2_fs_fsck_errcode(c: *mut c::bch_fs, msg: *mut c::printbuf) -> c_int;
}
