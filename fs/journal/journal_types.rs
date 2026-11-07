// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::errcode::{ret_to_result_void, BchError};
use crate::fs::Fs;
use crate::types::c_default;
use crate::util::closure::{closure_set_waiting, closure_unset_waiting, closure_wake_up};
use crate::util::{Printbuf, ToText};
use core::ffi::{c_int, c_uint};
use core::mem::ManuallyDrop;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicPtr, Ordering::{Relaxed, SeqCst}};
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
        self.reservations().cur_entry_offset() == c::JOURNAL_ENTRY_ERROR_VAL
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

/// Flush waiters swapped off a buf's word: a chain, ended by NULL, that
/// nothing else reaches until it's woken - when it drops - or added to
/// another buf's.
pub struct Waiters {
    first: NonNull<c::llist_node>,
    last:  NonNull<c::llist_node>,
}

impl Waiters {
    /// # Safety
    /// @first is a chain swapped off a buf's word: no one else's.
    unsafe fn taken(first: NonNull<c::llist_node>) -> Self {
        let mut last = first;
        while let Some(next) = NonNull::new(unsafe { last.as_ref() }.next) {
            last = next;
        }
        Waiters { first, last }
    }

    fn set_tail(&mut self, next: *mut c::llist_node) {
        unsafe { (*self.last.as_ptr()).next = next };
    }

    fn into_raw(self) -> *mut c::llist_node {
        ManuallyDrop::new(self).first.as_ptr()
    }

    pub fn wake(self) {}
}

impl Drop for Waiters {
    fn drop(&mut self) {
        closure_wake_up(&c::closure_waitlist { list: c::llist_head { first: self.first.as_ptr() } });
    }
}

/// Where journal_buf::wait() left its closure.
pub enum BufWait {
    /// Not in flight: there's nothing from here on to wait for.
    NotInFlight,
    /// Decided noflush, or the clean->dirty FLUSH_NO_WAIT entry: not a flush
    /// to wait on.
    NotWaitable,
    /// Waiting on its flush - the first, or behind others.
    Waiting { first: bool },
}

impl c::journal_buf {
    pub(crate) fn wait_word(&self) -> &AtomicPtr<c::llist_node> {
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

    /// Its waiters, if it has any, the word left @with.
    pub fn take_waiters(&self, with: *mut c::llist_node) -> Option<Waiters> {
        let first = self.wait_word().swap(with, SeqCst);
        buf_waiters(first).then(|| unsafe { Waiters::taken(NonNull::new_unchecked(first)) })
    }

    /// @waiters on its word, ahead of any it has - back, if it isn't taking
    /// flush waiters: decided noflush, not in flight, or FLUSH_NO_WAIT's.
    pub fn add_waiters(&self, mut waiters: Waiters) -> Result<(), Waiters> {
        let w = self.wait_word();
        let mut old = w.load(Relaxed);
        loop {
            if !old.is_null() && old <= JOURNAL_BUF_FLUSH_NO_WAIT {
                waiters.set_tail(ptr::null_mut());
                return Err(waiters);
            }

            waiters.set_tail(if buf_waiters(old) { old } else { ptr::null_mut() });

            match w.compare_exchange(old, waiters.first.as_ptr(), SeqCst, Relaxed) {
                Ok(_) => {
                    waiters.into_raw();
                    return Ok(());
                }
                Err(o) => old = o,
            }
        }
    }

    /*
     * Variant of closure_wait() that respects the journal_buf sentinel states:
     * if the journal_buf is not waitable, no waiters can attach. Otherwise the
     * closure is attached to buf->wait; the act of attaching transitions buf
     * out of the "may be noflush" state into "must flush" (via the wait-list
     * head becoming a real pointer).
     */
    /// # Safety
    /// @buf is an in_flight slot, live or free - only its wait word is read,
    /// the one thing a free slot holds; @cl is a live closure, waiting on
    /// nothing.
    pub unsafe fn wait(buf: *const Self, cl: NonNull<c::closure>) -> BufWait {
        let w = unsafe { AtomicPtr::from_ptr((&raw const (*buf).wait.list.first).cast_mut()) };
        let refused = |first: *mut c::llist_node| (!first.is_null() && first <= JOURNAL_BUF_FLUSH_NO_WAIT)
            .then(|| if first == JOURNAL_BUF_NOT_IN_FLIGHT { BufWait::NotInFlight } else { BufWait::NotWaitable });

        /* Optimistic check: bail without touching closure state if not waitable */
        let mut first = w.load(Relaxed);
        if let Some(r) = refused(first) {
            return r;
        }

        /* Commit to waiting */
        let node = unsafe { closure_set_waiting(cl, crate::ret_ip!()) };

        /*
         * cmpxchg-loop attach, bailing on race-to-poison. Standard llist_add
         * would happily chain through the NOFLUSH sentinel, which would
         * corrupt the list — so we open-code the attach to check first each
         * iteration.
         */
        loop {
            if let Some(r) = refused(first) {
                /* Became non-waitable after we committed waiting state — roll
                 * back. Safe because cl isn't on any list yet, so no concurrent
                 * writer can touch cl->remaining. */
                unsafe { closure_unset_waiting(cl) };
                return r;
            }
            unsafe { (*node.as_ptr()).next = if buf_waiters(first) { first } else { ptr::null_mut() } };

            match w.compare_exchange(first, node.as_ptr(), SeqCst, Relaxed) {
                Ok(_) => return BufWait::Waiting { first: !buf_waiters(first) },
                Err(o) => first = o,
            }
        }
    }

    /// @waiters back, as its only ones: its word as it was before
    /// take_waiters() left @with.
    pub fn put_back_waiters(&self, waiters: Waiters, with: *mut c::llist_node) {
        let was = self.wait_word().swap(waiters.into_raw(), SeqCst);
        assert!(was == with, "journal buf's flush word changed while its waiters were away");
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

/* idx follows cur_entry_offset's 22 bits, the buf counts idx's 2 */
const STATE_IDX_SHIFT: u32 = c::JOURNAL_ENTRY_OFFSET_MAX.count_ones();

impl c::journal_res_state {
    pub fn from_v(v: u64) -> Self {
        c::journal_res_state { word: c::journal_res_state_word { v } }
    }

    pub fn v(self) -> u64 {
        unsafe { self.word.v }
    }

    fn count_shift(idx: u32) -> u32 {
        c::JOURNAL_STATE_BUF0_SHIFT + idx * c::JOURNAL_STATE_BUF_COUNT_BITS
    }

    pub fn count(self, idx: u32) -> u32 {
        (self.v() >> Self::count_shift(idx)) as u32 & c::JOURNAL_STATE_BUF_COUNT_MAX
    }

    /// Where the open entry's next reservation goes, in u64s - or one of the
    /// JOURNAL_ENTRY_*_VALs, for no entry open.
    pub fn cur_entry_offset(self) -> u32 {
        (self.v() & u64::from(c::JOURNAL_ENTRY_OFFSET_MAX)) as u32
    }

    pub fn set_cur_entry_offset(&mut self, offset: u32) {
        let mask = u64::from(c::JOURNAL_ENTRY_OFFSET_MAX);
        *self = Self::from_v(self.v() & !mask | u64::from(offset) & mask);
    }

    /// The ring slot of the entry open, or last open.
    pub fn idx(self) -> u32 {
        (self.v() >> STATE_IDX_SHIFT) as u32 & c::JOURNAL_STATE_BUF_MASK
    }

    pub fn is_open(self) -> bool {
        self.cur_entry_offset() < c::JOURNAL_ENTRY_CLOSED_VAL
    }

    /// A ref on idx()'s buf - false, and none, if its count is full:
    /// journal_state_inc().
    pub fn inc(&mut self) -> bool {
        let idx = self.idx();
        if self.count(idx) == c::JOURNAL_STATE_BUF_COUNT_MAX {
            return false;
        }
        *self = Self::from_v(self.v() + (1 << Self::count_shift(idx)));
        true
    }
}

const _: () = assert!(STATE_IDX_SHIFT + c::JOURNAL_STATE_BUF_MASK.count_ones() == c::JOURNAL_STATE_BUF0_SHIFT,
                      "journal_res_state: idx between cur_entry_offset and the counts");

impl c::journal {
    /// j->reservations, as it is now.
    pub fn reservations(&self) -> c::journal_res_state {
        c::journal_res_state::from_v(self.lock.hot.reservations.load(Relaxed))
    }

    /// A ref put on ring slot @idx's buf: the state after - journal_state_buf_put().
    pub fn state_buf_put(&self, idx: u32) -> c::journal_res_state {
        let d = 1u64 << c::journal_res_state::count_shift(idx);
        c::journal_res_state::from_v(self.lock.hot.reservations.fetch_sub(d, SeqCst) - d)
    }

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

    /// What every journal entry reserves for @res, made @u64s.
    pub fn journal_entry_res_resize(&self, res: &c::journal_entry_res, u64s: c_uint) {
        unsafe { bch2_journal_entry_res_resize(self.journal.as_ptr(), ptr::from_ref(res).cast_mut(), u64s) }
    }

    /// Advance the rewind limit to @seq: buckets emptied before it may be
    /// discarded, once a journal flush has the limit on disk.
    pub fn journal_advance_rewind_seq(&self, seq: u64) {
        unsafe { bch2_journal_advance_rewind_seq(self.journal.as_ptr(), seq) }
    }

    /// Start writes of whatever's ready to be written.
    pub fn journal_do_writes(&self) {
        self.journal.do_writes()
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
