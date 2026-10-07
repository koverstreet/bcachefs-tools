// SPDX-License-Identifier: GPL-2.0

//! The data types of journal/seq_blacklist_format.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use zerocopy::byteorder::little_endian as le;
use cstruct_macros::CStruct;
use typeinfo_macros::TypeInfo;

#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct)]
pub struct journal_seq_blacklist_entry {
    pub start: le::U64,
    pub end: le::U64,
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_sb_field_journal_seq_blacklist {
    pub field: c::bch_sb_field,
    pub start: [c::journal_seq_blacklist_entry; 0],
}
