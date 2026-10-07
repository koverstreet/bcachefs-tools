// SPDX-License-Identifier: GPL-2.0

//! The data types of sb/downgrade_format.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use zerocopy::byteorder::little_endian as le;
use cstruct_macros::CStruct;
use typeinfo_macros::TypeInfo;

#[repr(C, align(2))]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
#[c_packed]
pub struct bch_sb_field_downgrade_entry {
    pub version: le::U16,
    pub recovery_passes: [le::U64; 2],
    pub nr_errors: le::U16,
    #[c("__le16 errors[] __counted_by(nr_errors)")]
    pub errors: [le::U16; 0],
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_sb_field_downgrade {
    pub field: c::bch_sb_field,
    pub entries: [c::bch_sb_field_downgrade_entry; 0],
}
