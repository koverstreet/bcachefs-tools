// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::util::vstructs::{vstruct, Ffi};
use cstruct_macros::{unsized_vstruct, CStruct, Plain, TypeInfo};

#[unsized_vstruct]
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
pub struct bch_sb_field_extent_type_u64s {
    pub field: Ffi<c::bch_sb_field>,
    pub d: [u8; 0],
}
vstruct!(unsized bch_sb_field_extent_type_u64s, field.u64s, start = 0);
