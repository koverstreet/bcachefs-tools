// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::DArray;
use cstruct_macros::{CStruct, c_extern, c_typedef};

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
#[c_typedef(u64_range)]
pub struct u64_range {
    pub start: u64,
    pub end: u64,
}

c_typedef! {
    pub type darray_u64_range = DArray<c::u64_range>;
}

c_extern! {
    pub fn bch2_journal_pos_from_member_info_set(c: *mut c::bch_fs);
    pub fn bch2_journal_ptrs_to_text(out: *mut c::printbuf, c: *mut c::bch_fs,
                                     j: *mut c::journal_replay);
    pub fn bch2_journal_entry_missing_range(c: *mut c::bch_fs, start: u64,
                                            end: u64) -> c::u64_range;
}
