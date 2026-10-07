// SPDX-License-Identifier: GPL-2.0

//! The data types of journal/read_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::DArray;
use cstruct_macros::{CStruct, c_extern, c_verbatim};

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
#[c_typedef(u64_range)]
pub struct u64_range {
    pub start: u64,
    pub end: u64,
}

c_verbatim!(r#"
DEFINE_DARRAY(u64_range);
"#);

pub type darray_u64_range = DArray<c::u64_range>;

// What Rust calls of journal/read.h: C gets these as prototypes, in journal/read_gen.h
// with the types - see c_extern! in types/cstruct.rs.
c_extern! {
    pub fn bch2_journal_ptrs_to_text(arg1: *mut c::printbuf, arg2: *mut c::bch_fs, arg3: *mut c::journal_replay);
    pub fn bch2_journal_entry_missing_range(arg1: *mut c::bch_fs, arg2: u64, arg3: u64) -> c::u64_range;
}
