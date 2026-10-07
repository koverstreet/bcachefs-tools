// SPDX-License-Identifier: GPL-2.0

//! The data types of data/nocow_locking_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::{c_const, CStruct};

c_const! {
    #[c_int]
    pub const BUCKET_NOCOW_LOCKS_BITS: u32 = 10;
}

c_const! {
    pub const BUCKET_NOCOW_LOCKS: u32 = 1 << c::BUCKET_NOCOW_LOCKS_BITS;
}

c_const! {
    #[c_int]
    pub const NOCOW_LOCK_BUCKET_SIZE: u32 = 6;
}

#[repr(C)]
#[derive(CStruct)]
#[c_align("SMP_CACHE_BYTES")]
pub struct nocow_lock_bucket {
    #[c_anon("")] pub __align: [crate::types::CacheAligned; 0],
    pub wait: c::closure_waitlist,
    pub lock: c::spinlock_t,
    pub b: [u64; c::NOCOW_LOCK_BUCKET_SIZE as usize],
    pub l: [c::atomic_t; c::NOCOW_LOCK_BUCKET_SIZE as usize],
}
c_default!(nocow_lock_bucket);

#[repr(C)]
#[derive(CStruct)]
pub struct bucket_nocow_lock_table {
    pub l: [c::nocow_lock_bucket; c::BUCKET_NOCOW_LOCKS as usize],
}
c_default!(bucket_nocow_lock_table);
