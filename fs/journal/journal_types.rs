// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::errcode::{ret_to_result_void, BchError};
use crate::fs::Fs;
use crate::types::c_default;
use crate::util::{Printbuf, ToText};
use core::ffi::{c_int, c_uint};
use core::ptr;
use core::sync::atomic::{AtomicPtr, Ordering::{self, Relaxed, SeqCst}};
use cstruct_macros::{CStruct, c_enum, c_export, c_extern};

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
    pub fn bch2_journal_cycle(j: *mut c::journal, flags: c::journal_cycle_flags);
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

/*
 * buf->wait.list.first is the write's flush decision, and its flush waiters:
 * journal.h's JOURNAL_BUF_* sentinels, which C names too while journal.c is C
 */
pub const JOURNAL_BUF_NOT_IN_FLIGHT: *mut c::llist_node = 1 as _;
pub const JOURNAL_BUF_NOFLUSH: *mut c::llist_node = 2 as _;
pub const JOURNAL_BUF_FLUSH_NO_WAIT: *mut c::llist_node = 3 as _;
pub const JOURNAL_BUF_FLUSH: *mut c::llist_node = 4 as _;

/* Real waiters, not one of the sentinels: */
pub fn buf_waiters(first: *mut c::llist_node) -> bool {
    first > JOURNAL_BUF_FLUSH
}

impl c::journal_buf {
    fn wait_word(&self) -> &AtomicPtr<c::llist_node> {
        unsafe { AtomicPtr::from_ptr((&raw const self.wait.list.first).cast_mut()) }
    }

    pub fn must_flush(&self) -> bool {
        /*
         * "must be written as a flush" - true for entries with real flush
         * waiters AND for the FLUSH_NO_WAIT transition entry (which has no
         * waiters but must still flush, to mark the fs dirty). The close/cycle
         * logic keys on this, so FLUSH_NO_WAIT must count here or its entry
         * never gets closed and written.
         */
        self.wait_word().load(Relaxed) > JOURNAL_BUF_NOFLUSH
    }

    pub fn try_noflush(&self) -> bool {
        let w = self.wait_word();
        let mut old = w.load(Relaxed);
        loop {
            if old == JOURNAL_BUF_NOFLUSH {
                return true;
            }
            if !old.is_null() {
                return false;
            }
            match w.compare_exchange(old, JOURNAL_BUF_NOFLUSH, SeqCst, Relaxed) {
                Ok(_) => return true,
                Err(o) => old = o,
            }
        }
    }
}

impl c::JournalLocked {
    /*
     * Look up the buffer for @seq via the in_flight FIFO: under j->lock, since
     * the FIFO front moves with seq_ondisk and buffers are freed on pop.
     *
     * A reservation holder doesn't need its buf: journal_res_data() and
     * ring_has_overwrites are its ring slot's, pinned by the reservation.
     */
    pub fn seq_to_buf(&self, seq: u64) -> Option<&c::journal_buf> {
        self.in_flight.get(seq)
    }

    pub fn seq_to_buf_mut(&mut self, seq: u64) -> Option<&mut c::journal_buf> {
        self.in_flight.get_mut(seq)
    }
}

impl c::journal_res_state {
    pub fn count(self, idx: u32) -> u32 {
        (unsafe { self.word.v } >> (c::JOURNAL_STATE_BUF0_SHIFT + idx * c::JOURNAL_STATE_BUF_COUNT_BITS)) as u32
            & c::JOURNAL_STATE_BUF_COUNT_MAX
    }
}

impl c::journal {
    /// @seq's buf's reservation count in @s, 0 for a seq too old to have one.
    pub fn state_seq_count(&self, s: c::journal_res_state, seq: u64) -> u32 {
        if self.fs().journal_cur_seq().wrapping_sub(seq) < u64::from(c::JOURNAL_STATE_BUF_NR) {
            s.count((seq & u64::from(c::JOURNAL_STATE_BUF_MASK)) as u32)
        } else {
            0
        }
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
        self.journal.reclaim_to_text(out)
    }

    /* journal_replicas, or metadata_replicas when it isn't set: */
    pub fn journal_replicas(&self) -> u32 {
        let opts = self.opts();
        match opts.journal_replicas.get() {
            0 => opts.metadata_replicas.get().into(),
            r => r.into(),
        }
    }
}

#[c_export]
pub fn bch2_journal_replicas(c: &Fs) -> c_uint {
    c.journal_replicas()
}

#[c_export]
pub fn journal_wake(j: &c::journal) {
    j.wake_async_wait()
}

/// # Safety
/// The caller holds j->lock.
#[c_export]
pub unsafe fn journal_seq_to_buf(j: &c::journal, seq: u64) -> *mut c::journal_buf {
    ebug_assert!(seq <= j.fs().journal_cur_seq(), "journal buf for seq {seq}, past the current");
    let mut g = unsafe { j.lock.held() };
    g.seq_to_buf_mut(seq).map_or(ptr::null_mut(), ptr::from_mut)
}

#[c_export]
pub fn journal_state_seq_count(j: &c::journal, s: c::journal_res_state, seq: u64) -> c_int {
    j.state_seq_count(s, seq) as c_int
}

#[c_export]
pub fn journal_buf_waiters(first: *mut c::llist_node) -> bool {
    buf_waiters(first)
}

#[c_export]
pub fn journal_buf_must_flush(buf: &c::journal_buf) -> bool {
    buf.must_flush()
}

#[c_export]
pub fn journal_buf_try_noflush(buf: &c::journal_buf) -> bool {
    buf.try_noflush()
}
