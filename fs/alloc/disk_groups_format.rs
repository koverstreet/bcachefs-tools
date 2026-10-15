// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::Le64;
use crate::util::vstructs::{vstruct, Head};
use cstruct_macros::{c_bitmask, c_const, unsized_vstruct, CStruct, Plain, TypeInfo};

c_const! {
    #[c_int]
    pub const BCH_SB_LABEL_SIZE: u32 = 32;
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
#[c_packed]
pub struct bch_disk_group {
    pub label: [u8; c::BCH_SB_LABEL_SIZE as usize],
    pub flags: [Le64; 2],
}

c_bitmask! {
    LE64_BITMASK(struct bch_disk_group, flags[0]) {
        BCH_GROUP_DELETED(0, 1),
        BCH_GROUP_DATA_ALLOWED(1, 6),
        BCH_GROUP_PARENT(6, 24),
    }
}

#[unsized_vstruct]
#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
#[c_packed]
pub struct bch_sb_field_disk_groups {
    pub field: Head<c::bch_sb_field>,
    pub entries: [c::bch_disk_group; 0],
}
vstruct!(unsized bch_sb_field_disk_groups, field.u64s, start = 0);
