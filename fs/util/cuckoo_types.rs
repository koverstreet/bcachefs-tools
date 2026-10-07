// SPDX-License-Identifier: GPL-2.0

//! The data types of util/cuckoo_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::{c_const, CStruct};

c_const! {
    #[c_int]
    pub const CUCKOO_NR_HASH: u32 = 3;
}

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct cuckoo_u64 {
    pub seeds: [u64; c::CUCKOO_NR_HASH as usize],
    pub bits: core::ffi::c_uint,
    pub nr: usize,
    pub stash: u64,
    pub d: *mut u64,
}
c_default!(cuckoo_u64);
