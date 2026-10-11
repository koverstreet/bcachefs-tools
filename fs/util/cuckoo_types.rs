// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use core::ffi::c_uint;
use cstruct_macros::{c_const, CStruct};

c_const! {
    #[c_int]
    pub const CUCKOO_NR_HASH: u32 = 3;
}

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct cuckoo_u64 {
    pub seeds: [u64; c::CUCKOO_NR_HASH as usize],
    pub bits: c_uint,
    pub nr: usize,
    pub stash: u64,
    pub d: *mut u64,
}
c_default!(cuckoo_u64);
