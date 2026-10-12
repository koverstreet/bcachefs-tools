// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::util::vstructs::vstruct;
use cstruct_macros::{CStruct, Plain, TypeInfo, View};

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo, View)]
pub struct bch_sb_field_extent_type_u64s {
    pub field: c::bch_sb_field,
    pub d: [u8; 0],
}
vstruct!(bch_sb_field_extent_type_u64s, field.u64s, start = 0);
