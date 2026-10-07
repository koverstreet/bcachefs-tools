// SPDX-License-Identifier: GPL-2.0

//! The data types of journal/reclaim_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::DArray;
use cstruct_macros::{CStruct, c_extern, c_verbatim};

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
#[c_typedef]
pub struct replicas_entry_refs {
    pub nr_refs: core::ffi::c_uint,
    pub replicas: c::bch_replicas_padded,
}

c_verbatim!(r#"
DEFINE_DARRAY_PREALLOCATED(replicas_entry_refs, 16);
"#);

pub type darray_replicas_entry_refs = DArray<c::replicas_entry_refs, 16>;

// What Rust calls of journal/reclaim.h: C gets these as prototypes, in journal/reclaim_gen.h
// with the types - see c_extern! in types/cstruct.rs.
c_extern! {
    pub fn bch2_journal_flush_pins(arg1: *mut c::journal, arg2: u64) -> bool;
}
