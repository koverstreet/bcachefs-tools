// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::{CStruct, c_const, TypeInfo};

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
}
c_default!(bch_fs_counters);
