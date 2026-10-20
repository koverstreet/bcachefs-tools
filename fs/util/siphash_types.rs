// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use core::ffi::{c_int, c_void};
use crate::types::Le64;
use cstruct_macros::{c_const, c_extern, CStruct, Plain};

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

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, Plain)]
#[c_typedef]
pub struct SIPHASH_KEY {
    pub k0: Le64,
    pub k1: Le64,
}

c_extern! {
    pub fn SipHash_Init(ctx: *mut c::_SIPHASH_CTX, key: *const c::SIPHASH_KEY);
    pub fn SipHash_Update(ctx: *mut c::_SIPHASH_CTX, rc: c_int, rf: c_int, src: *const c_void,
                          len: usize);
    pub fn SipHash_End(ctx: *mut c::_SIPHASH_CTX, rc: c_int, rf: c_int) -> u64;
}
