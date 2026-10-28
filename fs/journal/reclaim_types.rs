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
    pub fn __bch2_journal_set_alloc_watermark(j: *mut c::journal, p: *mut u8, watermark: c_uint);
}

impl c::journal {
    /// The open bucket pool can't serve below @watermark - 0 for all of them:
    /// the journal throttles work that would need what it can't have. The
    /// unchanged case a compare, as C's bch2_journal_set_alloc_watermark()
    /// - the allocator's fast paths report on every bucket.
    pub fn set_open_buckets_watermark(&self, watermark: u32) {
        if u32::from(self.watermark_open_buckets.get()) != watermark {
            // SAFETY: ours, and its field - a Volatile<u8>, laid out as a u8 -
            // set under the journal's lock, which C takes
            unsafe {
                __bch2_journal_set_alloc_watermark(core::ptr::from_ref(self).cast_mut(),
                    (&raw const self.watermark_open_buckets).cast_mut().cast(), watermark)
            }
        }
    }
}
