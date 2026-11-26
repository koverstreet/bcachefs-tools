// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::types::Le64;
use cstruct_macros::{whole, MaybeZeroable, Plain, TypeInfo};

#[repr(C)]
#[derive(Clone, Copy, Default, MaybeZeroable, Plain, TypeInfo)]
pub struct JournalSeqBlacklistEntry {
    pub start: Le64,
    pub end: Le64,
}

#[whole(tail = start: [JournalSeqBlacklistEntry])]
#[repr(C)]
#[derive(Clone, Copy, Default, MaybeZeroable, Plain, TypeInfo)]
pub struct SbFieldJournalSeqBlacklist {
    pub start: [JournalSeqBlacklistEntry; 0],
}
