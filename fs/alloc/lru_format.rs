// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::Le64;
use cstruct_macros::{bch_enum, MaybeZeroable, Plain, TypeInfo, SizedVal};

#[repr(C, align(8))]
#[derive(Clone, Copy, Default, MaybeZeroable, Plain, TypeInfo, SizedVal)]
pub struct Lru {
    pub v: c::bch_val,
    pub idx: Le64,
}

#[bch_enum(open)]
#[repr(u32)]
pub enum LruType {
    read = 0,
    fragmentation = 1,
    stripes = 2,
}

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

pub const LRU_TIME_BITS: u32 = 48;
pub const LRU_TIME_MAX: u64 = (1 << c::LRU_TIME_BITS) - 1;
