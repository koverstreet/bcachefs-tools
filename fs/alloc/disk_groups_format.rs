// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::types::Le64;
use cstruct_macros::{bitfield, whole, MaybeZeroable, Plain, TypeInfo};

pub const BCH_SB_LABEL_SIZE: u32 = 32;

#[repr(C, align(8))]
#[derive(Clone, Copy, Default, MaybeZeroable, Plain, TypeInfo)]
pub struct bch_disk_group {
    pub label: [u8; BCH_SB_LABEL_SIZE as usize],
    pub flags0: bch_disk_group_flags,
    pub flags1: Le64,
}

#[bitfield(Le64)]
pub struct bch_disk_group_flags {
    deleted:                    bool,
    data_allowed:               u5,
    parent:                     u18,
    __:                         u40,
}

#[whole(tail = entries: [bch_disk_group])]
#[repr(C, align(8))]
#[derive(Clone, Copy, Default, MaybeZeroable, Plain, TypeInfo)]
pub struct bch_sb_field_disk_groups {
    pub entries: [bch_disk_group; 0],
}
