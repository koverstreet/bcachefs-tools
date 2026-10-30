// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use cstruct_macros::{CStruct, Plain, TypeInfo};

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
pub struct bch_member_cpu {
    pub nbuckets: u64, /* device size */
    pub nbuckets_minus_first: u64,
    pub first_bucket: u16, /* index of first bucket used */
    pub bucket_size: u16, /* sectors */
    pub group: u16,
    pub failure_domain: u16,
    pub state: u8,
    pub discard: u8,
    pub data_allowed: u8,
    pub durability: u8,
    pub freespace_initialized: u8,
    pub initialized: u8,
    pub resize_on_mount: u8,
    pub rotational: u8,
    pub valid: u8,
    pub btree_bitmap_shift: u8,
    /* explicit: copied a word at a time, it has no padding to copy */
    pub __pad: [u8; 6],
    pub btree_allocated_bitmap: u64,
}

// ca->mi: read by anyone, a field at a time - a copy half made is one
crate::tear_safe!(bch_member_cpu);
