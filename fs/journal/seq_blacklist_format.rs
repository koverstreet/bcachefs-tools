// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::Le64;
use crate::util::vstructs::{vstruct, Head};
use cstruct_macros::{unsized_vstruct, CStruct, Plain, TypeInfo};

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
pub struct journal_seq_blacklist_entry {
    pub start: Le64,
    pub end: Le64,
}

#[unsized_vstruct]
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
pub struct bch_sb_field_journal_seq_blacklist {
    pub field: Head<c::bch_sb_field>,
    pub start: [c::journal_seq_blacklist_entry; 0],
}
vstruct!(unsized bch_sb_field_journal_seq_blacklist, field.u64s, start = 0);
