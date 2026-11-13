// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::Le64;
use crate::util::vstructs::{vstruct, Head};
use cstruct_macros::{bitfield, unsized_vstruct, MaybeZeroable, Plain, TypeInfo};

pub const BCH_SB_LABEL_SIZE: u32 = 32;

#[repr(C, align(8))]
#[derive(Clone, Copy, Default, MaybeZeroable, Plain, TypeInfo)]
pub struct bch_disk_group {
    pub label: [u8; c::BCH_SB_LABEL_SIZE as usize],
    pub flags0: bch_disk_group_flags,
    pub flags1: Le64,
}

#[bitfield(Le64)]
pub struct bch_disk_group_flags {
    deleted:                    bool,
    #[bits(5)]  data_allowed:   u64,
    #[bits(18)] parent:         u64,
    #[bits(40)] __:             u64,
}

#[unsized_vstruct]
#[repr(C, align(8))]
#[derive(Clone, Copy, Default, MaybeZeroable, Plain, TypeInfo)]
pub struct bch_sb_field_disk_groups {
    pub field: Head<c::bch_sb_field>,
    pub entries: [c::bch_disk_group; 0],
}
vstruct!(unsized bch_sb_field_disk_groups, field.u64s, start = 0);
