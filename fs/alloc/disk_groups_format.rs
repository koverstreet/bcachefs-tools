// SPDX-License-Identifier: GPL-2.0

//! The data types of alloc/disk_groups_format.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use zerocopy::byteorder::little_endian as le;
use cstruct_macros::{c_bitmask, c_const, CStruct};
use typeinfo_macros::TypeInfo;

c_const! {
    #[c_int]
    pub const BCH_SB_LABEL_SIZE: u32 = 32;
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
#[c_packed]
pub struct bch_disk_group {
    pub label: [u8; c::BCH_SB_LABEL_SIZE as usize],
    pub flags: [le::U64; 2],
}

c_bitmask! {
    LE64_BITMASK(struct bch_disk_group, flags[0]) {
        BCH_GROUP_DELETED(0, 1),
        BCH_GROUP_DATA_ALLOWED(1, 6),
        BCH_GROUP_PARENT(6, 24),
    }
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
#[c_packed]
pub struct bch_sb_field_disk_groups {
    pub field: c::bch_sb_field,
    pub entries: [c::bch_disk_group; 0],
}
