// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::types::Le64;
use cstruct_macros::{whole, MaybeZeroable, Plain, TypeInfo};

#[repr(C)]
#[derive(Clone, Copy, Default, MaybeZeroable, Plain, TypeInfo)]
pub struct journal_seq_blacklist_entry {
    pub start: Le64,
    pub end: Le64,
}

#[whole(tail = start: [journal_seq_blacklist_entry])]
#[repr(C)]
#[derive(Clone, Copy, Default, MaybeZeroable, Plain, TypeInfo)]
pub struct bch_sb_field_journal_seq_blacklist {
    pub start: [journal_seq_blacklist_entry; 0],
}
