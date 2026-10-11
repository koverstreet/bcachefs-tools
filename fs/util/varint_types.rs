// SPDX-License-Identifier: GPL-2.0

//! What Rust calls of util/varint.h - C gets it as prototypes, in util/varint_gen.h,
//! which is generated from this file: see fs/types/lib.rs.

#![allow(non_camel_case_types)]

use core::ffi::c_int;
use cstruct_macros::c_extern;

c_extern! {
    pub fn bch2_varint_encode(arg1: *mut u8, arg2: u64) -> c_int;
    pub fn bch2_varint_decode(arg1: *const u8, arg2: *const u8, arg3: *mut u64) -> c_int;
}
