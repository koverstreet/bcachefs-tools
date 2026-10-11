// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::Le64;
use cstruct_macros::CStruct;
use typeinfo_macros::TypeInfo;

#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct)]
pub struct journal_seq_blacklist_entry {
    pub start: Le64,
    pub end: Le64,
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_sb_field_journal_seq_blacklist {
    pub field: c::bch_sb_field,
    pub start: [c::journal_seq_blacklist_entry; 0],
}
