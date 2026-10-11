// SPDX-License-Identifier: GPL-2.0

//! What Rust calls of util/varint.h - C gets it as prototypes, in
//! util/varint_gen.h, which is generated from this file: see fs/types/lib.rs.

#![allow(non_camel_case_types)]

use core::ffi::c_int;
use cstruct_macros::c_extern;

c_extern! {
    pub fn bch2_varint_encode(out: *mut u8, v: u64) -> c_int;
    pub fn bch2_varint_decode(in_: *const u8, end: *const u8, out: *mut u64) -> c_int;
}
