// SPDX-License-Identifier: GPL-2.0

//! The data types of vfs/fdm_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::{c_const, CStruct};

c_const! {
    #[c_int]
    pub const FDM_NR_HASH: u32 = 3;
}

c_const! {
    #[c_int]
    pub const FDM_HASH_BITS: u32 = 9;
}

c_const! {
    #[c_int]
    pub const FDM_HASH_SIZE: u32 = 1 << c::FDM_HASH_BITS;
}

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct fdm_slot {
    pub task: *mut c::task_struct,
    pub mapping: core::ffi::c_ulong, /* address_space * | dropped_locks bit */
}
c_default!(fdm_slot);

#[repr(C)]
#[derive(CStruct)]
pub struct fdm_hash {
    pub hash_seeds: [u64; c::FDM_NR_HASH as usize],
    pub wait: c::closure_waitlist,
    pub slots: [c::fdm_slot; c::FDM_HASH_SIZE as usize],
}
c_default!(fdm_hash);
