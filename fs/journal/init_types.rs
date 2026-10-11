// SPDX-License-Identifier: GPL-2.0

//! What Rust calls of journal/init.h - C gets it as prototypes, in journal/init_gen.h,
//! which is generated from this file: see fs/types/lib.rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use core::ffi::{c_int, c_uint};
use cstruct_macros::c_extern;

c_extern! {
    pub fn bch2_set_nr_journal_buckets(arg1: *mut c::bch_fs, arg2: *mut c::bch_dev, arg3: c_uint) -> c_int;
}
