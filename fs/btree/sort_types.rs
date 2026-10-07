// SPDX-License-Identifier: GPL-2.0

//! The data types of btree/sort_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::{c_enum, CStruct};

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct sort_iter_set {
    pub k: *mut c::bkey_packed,
    pub end: *mut c::bkey_packed,
}
c_default!(sort_iter_set);

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct sort_iter {
    pub b: *mut c::btree,
    pub used: core::ffi::c_uint,
    pub size: core::ffi::c_uint,

    pub data: [c::sort_iter_set; 0],
}
c_default!(sort_iter);

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct sort_iter_stack {
    pub iter: c::sort_iter,
    #[c("struct sort_iter_set sets[MAX_BSETS + 1]")]
    pub sets: [c::sort_iter_set; c::MAX_BSETS as usize + 1],
}
c_default!(sort_iter_stack);

c_enum! {
    #[closed]
    pub enum compact_mode: u32 {
        COMPACT_LAZY,
        COMPACT_ALL,
    }
}
