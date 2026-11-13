// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use crate::util::projections::Volatile;
use core::ffi::c_ulong;
use cstruct_macros::{MaybeZeroable};

pub const FDM_NR_HASH: u32 = 3;

pub const FDM_HASH_BITS: u32 = 9;

pub const FDM_HASH_SIZE: u32 = 1 << c::FDM_HASH_BITS;

#[repr(C)]
#[derive(MaybeZeroable)]
pub struct fdm_slot {
    pub task: Volatile<usize>,
    pub mapping: Volatile<c_ulong>, /* address_space * | dropped_locks bit */
}
c_default!(fdm_slot);

#[repr(C)]
#[derive(MaybeZeroable)]
pub struct fdm_hash {
    pub hash_seeds: [u64; c::FDM_NR_HASH as usize],
    pub wait: c::closure_waitlist,
    pub slots: [c::fdm_slot; c::FDM_HASH_SIZE as usize],
}
c_default!(fdm_hash);
