// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::DArray;
use core::ffi::c_uint;
use cstruct_macros::{CStruct, c_extern, c_typedef};

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
#[c_typedef]
pub struct replicas_entry_refs {
    pub nr_refs: c_uint,
    pub replicas: c::bch_replicas_padded,
}

c_typedef! {
    pub type darray_replicas_entry_refs = DArray<c::replicas_entry_refs, 16>;
}

c_extern! {
    pub fn bch2_journal_flush_pins(j: *mut c::journal, seq_to_flush: u64) -> bool;
    pub fn bch2_journal_reclaim_to_text(out: *mut c::printbuf, j: *mut c::journal);
    pub fn bch2_journal_seq_pins_to_text(out: *mut c::printbuf, j: *mut c::journal, seq: *mut u64,
                                         pin_type: *mut c_uint) -> bool;
}
