// SPDX-License-Identifier: GPL-2.0

//! What Rust calls of util/varint.h - C gets it as prototypes, in util/varint_gen.h,
//! which is generated from this file: see fs/types/lib.rs.

#![allow(non_camel_case_types)]

use cstruct_macros::c_extern;

// What Rust calls of util/varint.h: C gets these as prototypes, in util/varint_gen.h
// with the types - see c_extern! in types/cstruct.rs.
c_extern! {
    pub fn bch2_varint_encode(arg1: *mut u8, arg2: u64) -> core::ffi::c_int;
    pub fn bch2_varint_decode(arg1: *const u8, arg2: *const u8, arg3: *mut u64) -> core::ffi::c_int;
}
