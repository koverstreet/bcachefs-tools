// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::errcode::{ret_to_result_void, BchError};
use crate::fs::Fs;
use crate::types::c_default;
use crate::util::{Printbuf, ToText};
use core::ffi::c_int;
use cstruct_macros::{CStruct, c_enum, c_extern};

c_enum! {
    #[flags]
    pub enum journal_cycle_flags: u32 {
        JOURNAL_CYCLE_must_close = 1 << 0,
        JOURNAL_CYCLE_must_open = 1 << 1,
        JOURNAL_CYCLE_force_close = 1 << 2,
    }
}

/* First bits for BCH_WATERMARK: */
c_enum! {
    #[closed]
    pub enum journal_res_flags: u32 {
        __JOURNAL_RES_GET_NONBLOCK = c::BCH_WATERMARK_BITS,
        __JOURNAL_RES_GET_CHECK,
    }
}

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct journal_block {
    pub j: *mut c::journal,
}
c_default!(journal_block);

c_extern! {
    pub fn bch2_journal_flush(j: *mut c::journal) -> c_int;
    pub fn bch2_journal_meta(j: *mut c::journal) -> c_int;
    pub fn bch2_journal_do_writes(j: *mut c::journal);
    pub fn bch2_journal_debug_to_text(out: *mut c::printbuf, j: *mut c::journal);
}

impl ToText for c::journal {
    fn to_text(&self, out: &mut Printbuf) {
        unsafe { bch2_journal_debug_to_text(out.as_raw(), core::ptr::from_ref(self).cast_mut()) }
    }
}

impl Fs {
    /// Write a journal entry with nothing in it but metadata, and wait for
    /// it to be flushed.
    pub fn journal_meta(&self) -> Result<(), BchError> {
        ret_to_result_void(unsafe { bch2_journal_meta(&raw mut (*self.as_ptr()).journal) })
    }

    /// Start writes of whatever's ready to be written.
    pub fn journal_do_writes(&self) {
        unsafe { bch2_journal_do_writes(&raw mut (*self.as_ptr()).journal) }
    }

    pub fn journal_reclaim_to_text(&self, out: &mut Printbuf) {
        unsafe { c::bch2_journal_reclaim_to_text(out.as_raw(), &raw mut (*self.as_ptr()).journal) }
    }
}
