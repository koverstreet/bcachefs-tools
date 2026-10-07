// SPDX-License-Identifier: GPL-2.0

//! The data types of util/siphash_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use zerocopy::byteorder::little_endian as le;
use cstruct_macros::{c_const, c_extern, CStruct};

c_const! {
    #[c_int]
    pub const SIPHASH_BLOCK_LENGTH: u32 = 8;
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
#[c_typedef(SIPHASH_CTX)]
pub struct _SIPHASH_CTX {
    pub v: [u64; 4],
    pub buf: [u8; c::SIPHASH_BLOCK_LENGTH as usize],
    pub bytes: u32,
}
pub type SIPHASH_CTX = _SIPHASH_CTX;

#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct)]
#[c_typedef]
pub struct SIPHASH_KEY {
    pub k0: le::U64,
    pub k1: le::U64,
}

// What Rust calls of util/siphash.h: C gets these as prototypes, in
// util/siphash_gen.h with the types - see c_extern! in types/cstruct.rs.
c_extern! {
    pub fn SipHash_Init(ctx: *mut c::_SIPHASH_CTX, key: *const c::SIPHASH_KEY);
    pub fn SipHash_Update(ctx: *mut c::_SIPHASH_CTX, rc: core::ffi::c_int, rf: core::ffi::c_int, src: *const core::ffi::c_void, len: usize);
    pub fn SipHash_End(ctx: *mut c::_SIPHASH_CTX, rc: core::ffi::c_int, rf: core::ffi::c_int) -> u64;
}
