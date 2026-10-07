// SPDX-License-Identifier: GPL-2.0

//! The data types of alloc/lru_format.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use zerocopy::byteorder::little_endian as le;
use cstruct_macros::{c_const, c_enum, c_xmacro, CStruct};
use typeinfo_macros::TypeInfo;

#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
#[c_packed]
pub struct bch_lru {
    pub v: c::bch_val,
    pub idx: le::U64,
}

c_xmacro! {
    BCH_LRU_TYPES(x) {
        (read),
        (fragmentation),
        (stripes),
    }
}

macro_rules! __bch_lru_type_0 {
    ([$($acc:tt)*] $(($n:tt)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[open]
            pub enum bch_lru_type: u32 {
                $($acc)*
                $([<BCH_LRU_ $n>],)*
            }
        }
    } };
}
BCH_LRU_TYPES!(__bch_lru_type_0 []);

c_const! {
    /*
     * LRU id space: read LRUs and bucket fragmentation LRUs are per-device,
     * id = range base + device index:
     */
    pub const BCH_LRU_READ_MAX: u32 = 1 << 13;
}

c_const! {
    pub const BCH_LRU_BUCKET_FRAGMENTATION_START: u32 = 1 << 13;
}

c_const! {
    pub const BCH_LRU_BUCKET_FRAGMENTATION_END: u32 = 2 << 13;
}

c_const! {
    pub const BCH_LRU_STRIPE_FRAGMENTATION: u32 = (1 << 16) - 2;
}

c_const! {
    /*
     * Obsolete: the single fs-wide bucket fragmentation lru, replaced by the
     * per-device fragmentation lrus in per_dev_fragmentation_lru; stale entries
     * are deleted by check_lrus:
     */
    pub const BCH_LRU_BUCKET_FRAGMENTATION_OLD: u32 = (1 << 16) - 1;
}

c_const! {
    #[c_int]
    pub const LRU_TIME_BITS: u32 = 48;
}

c_const! {
    pub const LRU_TIME_MAX: u64 = (1 << c::LRU_TIME_BITS) - 1;
}
