// SPDX-License-Identifier: GPL-2.0

//! The data types of sb/counters_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::{CStruct, c_const, c_extern};
use typeinfo_macros::TypeInfo;

c_const! {
    #[c_int]
    pub const NR_RECENT_COUNTERS: u32 = 20;
}

#[repr(C)]
#[derive(Clone, Copy, CStruct, TypeInfo)]
pub struct bch_fs_counters {
    pub mount: [u64; c::BCH_COUNTER_NR as usize],
    #[c("u64 __percpu *now")]
    pub now: *mut u64,

    pub recent: [[u64; c::BCH_COUNTER_NR as usize]; c::NR_RECENT_COUNTERS as usize],
    pub work: c::delayed_work,
}
c_default!(bch_fs_counters);

// What Rust calls of sb/counters.h: C gets these as prototypes, in sb/counters_gen.h
// with the types - see c_extern! in types/cstruct.rs.
c_extern! {
    pub fn bch2_counter_reset(arg1: *mut c::bch_fs, arg2: core::ffi::c_uint);
}
