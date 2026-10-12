// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::Le64;
use cstruct_macros::{c_const, c_enum, c_xmacro, CStruct, TypeInfo};

#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
#[c_packed]
pub struct bch_lru {
    pub v: c::bch_val,
    pub idx: Le64,
}

c_xmacro! {
    BCH_LRU_TYPES(x) {
        (read),
        (fragmentation),
        (stripes),
    }
}

c_enum! {
    #[open]
    pub enum bch_lru_type: u32 {
        BCH_LRU_TYPES(n) => BCH_LRU_ ## n,
    }
}

c_const! {
    /*
     * LRU id space: read LRUs and bucket fragmentation LRUs are per-device,
     * id = range base + device index:
     */
    pub const BCH_LRU_READ_MAX: u32 = 1 << 13;
    pub const BCH_LRU_BUCKET_FRAGMENTATION_START: u32 = 1 << 13;
    pub const BCH_LRU_BUCKET_FRAGMENTATION_END: u32 = 2 << 13;

    pub const BCH_LRU_STRIPE_FRAGMENTATION: u32 = (1 << 16) - 2;

    /*
     * Obsolete: the single fs-wide bucket fragmentation lru, replaced by the
     * per-device fragmentation lrus in per_dev_fragmentation_lru; stale entries
     * are deleted by check_lrus:
     */
    pub const BCH_LRU_BUCKET_FRAGMENTATION_OLD: u32 = (1 << 16) - 1;

    #[c_int]
    pub const LRU_TIME_BITS: u32 = 48;
    pub const LRU_TIME_MAX: u64 = (1 << c::LRU_TIME_BITS) - 1;
}
