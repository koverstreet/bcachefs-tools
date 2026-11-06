// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::errcode::{ret_to_result_void, BchError};
use crate::fs::Fs;
use crate::types::c_default;
use crate::util::{Printbuf, ToText};
use core::ffi::{c_int, c_uint};
use core::sync::atomic::Ordering;
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
    pub fn bch2_journal_flush_seq(j: *mut c::journal, seq: u64, task_state: c_uint) -> c_int;
    pub fn bch2_journal_entry_res_resize(j: *mut c::journal, res: *mut c::journal_entry_res, new_u64s: c_uint);
    pub fn bch2_journal_flush_async(j: *mut c::journal, parent: *mut c::closure);
    pub fn bch2_journal_advance_rewind_seq(j: *mut c::journal, seq: u64);
    pub fn bch2_journal_noflush_seq(j: *mut c::journal, start: u64, end: u64) -> bool;
    pub fn bch2_journal_meta(j: *mut c::journal) -> c_int;
    pub fn bch2_journal_do_writes(j: *mut c::journal);
    pub fn bch2_journal_debug_to_text(out: *mut Printbuf, j: *mut c::journal);
    pub fn bch2_journal_halt(j: *mut c::journal);
    pub fn bch2_journal_halt_locked(j: *mut c::journal);
    pub fn bch2_journal_add_rewind_range(c: *mut Fs, start: u64, end: u64) -> c_int;
    pub fn bch2_journal_write_work(work: *mut c::work_struct);
    pub fn __bch2_journal_meta(j: *mut c::journal) -> c_int;
    pub fn bch2_journal_shutdown_quiesce(j: *mut c::journal);
    pub fn bch2_journal_block(j: *mut c::journal);
    pub fn bch2_journal_unblock(j: *mut c::journal);
}

/// The journal blocked, as long as it lives: no reservations, the open entry
/// closed and its writes quiesced. Blocks nest.
pub struct JournalBlocked<'a>(&'a c::journal);

impl Drop for JournalBlocked<'_> {
    fn drop(&mut self) {
        unsafe { bch2_journal_unblock(self.0.as_ptr()) }
    }
}

impl c::journal {
    /// For C, which writes what it changes through it as its locks allow.
    pub fn as_ptr(&self) -> *mut c::journal {
        core::ptr::from_ref(self).cast_mut()
    }

    pub fn halted(&self) -> bool {
        self.lock.hot.reservations.load(Ordering::Relaxed) & u64::from(c::JOURNAL_ENTRY_OFFSET_MAX) ==
            u64::from(c::JOURNAL_ENTRY_ERROR_VAL)
    }

    pub fn block(&self) -> JournalBlocked<'_> {
        unsafe { bch2_journal_block(self.as_ptr()) };
        JournalBlocked(self)
    }

    /// journal_shutdown if it's halted: bch2_journal_error().
    pub fn error(&self) -> Result<(), BchError> {
        if self.halted() {
            Err(crate::errcode::bch_errcode::BCH_ERR_journal_shutdown.into())
        } else {
            Ok(())
        }
    }

    /// Everything committed before the call is on disk when it returns
    pub fn flush(&self) -> Result<(), BchError> {
        ret_to_result_void(unsafe { bch2_journal_flush(self.as_ptr()) })
    }

    pub fn flush_seq(&self, seq: u64, task_state: c_uint) -> Result<(), BchError> {
        ret_to_result_void(unsafe { bch2_journal_flush_seq(self.as_ptr(), seq, task_state) })
    }
}

impl ToText for c::journal {
    fn to_text(&self, out: &mut Printbuf) {
        unsafe { bch2_journal_debug_to_text(out.as_raw(), self.as_ptr()) }
    }
}

impl Fs {
    pub fn journal_add_rewind_range(&self, start: u64, end: u64) -> Result<(), BchError> {
        ret_to_result_void(unsafe { bch2_journal_add_rewind_range(self.as_ptr(), start, end) })
    }

    /// Write a journal entry with nothing in it but metadata, and wait for
    /// it to be flushed.
    pub fn journal_meta(&self) -> Result<(), BchError> {
        ret_to_result_void(unsafe { bch2_journal_meta(self.journal.as_ptr()) })
    }

    /// Ask the journal not to flush seqs [@start, @end), whose entries hold
    /// nothing that needs it - false if it can't: one's been flushed.
    pub fn journal_noflush_seq(&self, start: u64, end: u64) -> bool {
        unsafe { bch2_journal_noflush_seq(self.journal.as_ptr(), start, end) }
    }

    /// Advance the rewind limit to @seq: buckets emptied before it may be
    /// discarded, once a journal flush has the limit on disk.
    pub fn journal_advance_rewind_seq(&self, seq: u64) {
        unsafe { bch2_journal_advance_rewind_seq(self.journal.as_ptr(), seq) }
    }

    /// Start writes of whatever's ready to be written.
    pub fn journal_do_writes(&self) {
        unsafe { bch2_journal_do_writes(self.journal.as_ptr()) }
    }

    pub fn journal_reclaim_to_text(&self, out: &mut Printbuf) {
        unsafe { c::bch2_journal_reclaim_to_text(out.as_raw(), self.journal.as_ptr()) }
    }
}
