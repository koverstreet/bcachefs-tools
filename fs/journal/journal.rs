// SPDX-License-Identifier: GPL-2.0
/*
 * bcachefs journalling code, for btree insertions
 *
 * Copyright 2012 Google, Inc.
 */

/* DOC_LATEX(journal)
 * The journal is a write-ahead log for metadata. Instead of writing every
 * \hyperref[sec:btrees]{btree} update directly to its btree node on disk,
 * bcachefs records updates in the journal first. This allows metadata writes to be batched and sequential,
 * dramatically improving performance.
 *
 * \subsubsection{How the journal works}
 *
 * Each journal entry (\texttt{struct jset}) contains a list of typed sub-entries:
 * btree key updates, btree root pointers, timestamps, IO clock values, and
 * diagnostic messages. Entries are assigned monotonically increasing sequence
 * numbers that survive crashes and are never reused.
 *
 * The journal is stored as a ring buffer of buckets on each device. As new
 * entries are written, they advance through the ring. Old entries are reclaimed
 * once all the btree nodes they refer to have been flushed to disk.
 *
 * \subsubsection{Journal pins and reclaim}
 *
 * A \emph{journal pin} holds a reference from a dirty btree node (or key cache
 * entry) to the journal sequence that contains its latest update. Journal space
 * for a sequence cannot be reclaimed until all pins referencing that sequence
 * are released---which happens when the corresponding btree node is written to
 * disk.
 *
 * The journal reclaim thread runs in the background, identifying which btree
 * nodes are pinning the oldest journal sequences and flushing them. Under normal
 * operation this is invisible; under heavy write load, reclaim may need to work
 * harder to keep up.
 *
 * \subsubsection{Space pressure}
 *
 * When free journal space drops below 25\%, or the pin list fills to 75\%
 * capacity, the journal enters a reclaim watermark state. In this state:
 *
 * \begin{itemize}
 * \item New metadata writes may be throttled
 * \item The reclaim thread is woken to aggressively flush btree nodes
 * \item Space is typically freed within milliseconds as nodes flush
 * \end{itemize}
 *
 * If the journal fills completely, metadata operations block until space is
 * freed. This is rare under normal workloads and resolves automatically. A
 * sustained ``journal full'' condition typically indicates that btree node
 * writes are bottlenecked---often by a slow device or high IO contention.
 *
 * \subsubsection{Flush and ordering}
 *
 * Journal writes come in two flavors:
 *
 * \begin{description}
 * \item[Flush writes] Ordered to stable storage with disk cache flushes. These
 *   provide durability guarantees---data acknowledged to applications via
 *   \texttt{fsync()} is protected by flush writes. A configurable delay
 *   (\texttt{journal\_flush\_delay}, default 1000\,ms) batches updates before
 *   flushing.
 * \item[No-flush writes] Written without ordering guarantees. These can be lost
 *   on power failure but are much cheaper. Used between flush points to reduce
 *   IO overhead.
 * \end{description}
 *
 * On multi-device filesystems, flush writes issue a preflush to all devices
 * first, ensuring all pending data writes are ordered before the journal entry.
 *
 * \subsubsection{Mount and recovery}
 *
 * On mount, the journal is read from all devices. The recovery window is
 * determined by two sequence numbers: \texttt{last\_seq} (the oldest entry still
 * needed) and the sequence of the last valid flush entry. All entries in this
 * window are replayed in order, re-inserting their btree keys into the btree.
 * Journal replay is idempotent---replaying the same entry twice is safe.
 *
 * On clean shutdown, a special \texttt{clean} field is written to the
 * \hyperref[sec:superblock]{superblock} containing the btree roots and usage
 * counters, allowing the next mount to skip journal replay entirely.
 *
 * \textbf{Sequence blacklisting}: After an unclean shutdown, some btree nodes on
 * disk may reference journal sequences that were never durably committed. These
 * sequences are added to a blacklist stored in the superblock; any btree node
 * data referencing a blacklisted sequence is ignored during recovery. Once the
 * affected nodes are rewritten with new sequences, the blacklist entries are
 * garbage collected.
 *
 * \subsubsection{User-facing options}
 *
 * \begin{description}
 * \item[\texttt{journal\_flush\_delay}] Milliseconds before auto-committing the
 *   journal (default 1000). Lower values reduce the window of data loss on
 *   crash; higher values improve throughput.
 * \item[\texttt{journal\_flush\_disabled}] Disable journal flushes entirely.
 *   \textbf{Dangerous}---data loss is expected on any unclean shutdown.
 * \item[\texttt{journal\_reclaim\_delay}] Milliseconds before triggering
 *   background reclaim (default 100).
 * \item[\texttt{journal\_transaction\_names}] Log function names in journal
 *   entries for debugging (default enabled).
 * \end{description}
 *
 * Journal size is configured per device and can be resized online with
 * \texttt{bcachefs device resize-journal}.
 *
 * \subsubsection{Consistency and self-healing}
 *
 * Every journal entry is checksummed. Entries that fail checksum validation are
 * skipped during replay, with the filesystem falling back to the last known good
 * entry. The sequence blacklist mechanism ensures that partially-written state
 * from crashes cannot corrupt the btree. Journal entries are replicated across
 * devices according to the \texttt{metadata\_replicas} setting; if one device's
 * journal is unreadable, recovery proceeds from the other copies.
 */

use core::cell::Cell;
use core::ffi::{c_int, c_long, c_uint, c_ulong};
use core::mem;
use core::ptr::{self, NonNull};
use core::sync::atomic::{fence, AtomicPtr};
use core::sync::atomic::Ordering::{Acquire, Relaxed, SeqCst};


use crate::btree::iter::TransAttempt;
use crate::c;
use crate::c::JournalCycleFlags as Cycle;
use crate::c::JournalFlag as Journal;
use crate::c::JournalGuard;
use crate::journal::flush_wait::{BufWait, FlushEntry, FlushWait, Waiters, JOURNAL_BUF_FLUSH, JOURNAL_BUF_FLUSH_NO_WAIT,
                                 JOURNAL_BUF_NOFLUSH, JOURNAL_BUF_NOT_IN_FLIGHT};
use crate::debug::trace::events;
use crate::errcode::{Errcode, BchError};
use crate::format::SbFeature;
use crate::journal::pins::PinsRead;
use crate::data_structures::fifo::FifoRead;
use crate::locking::{Guard as _, MemallocFlags};
use crate::init::recovery::NotRw;
use crate::journal::entries::EntryWriter;
use crate::util::alloc::{flags, KVVec};
use crate::util::backtrace::prt_current_backtrace;
use crate::format::DataType;
use crate::util::task::{waiter, Select, WaitSync};
use crate::util::timer::Timer;
use crate::os::kernel::{jiffies, jiffies_to_msecs, local_clock, msecs_to_jiffies, time_after, unlikely, HZ, WRITE};
use crate::os::workqueue::Own;
use crate::util::futex::EventCount;
use crate::util::rcu::read_lock;
use crate::util::log::LogMsg;
use crate::util::sleep::Sleep;
use crate::util::printbuf::HumanReadable;
use crate::util::vstructs::Head;
use crate::util::refcount::TryRefcount;
use crate::util::{Printbuf, ToText};
use crate::{prt, warn_once};
use zerocopy::IntoBytes;

fn state_idx(seq: u64) -> u32 {
    (seq & u64::from(c::JOURNAL_STATE_BUF_MASK)) as u32
}

/// How the open entry is closed: to be written, putting its ref with the pins
/// held - or by a halt, which needn't hold them.
#[derive(Clone, Copy)]
enum Close<'a> {
    Closed(&'a PinsRead<'a>),
    Error,
}

/// The journal blocked, as long as it lives: no reservations, the open entry
/// closed and its writes quiesced. Blocks nest.
pub struct JournalBlocked<'a>(&'a c::journal);

impl Drop for JournalBlocked<'_> {
    fn drop(&mut self) {
        self.0.unblock()
    }
}

impl c::journal {
    fn cur_buf(&self, g: &JournalGuard<'_>) -> &c::journal_buf {
        self.in_flight.locked(g).get(self.fs().journal_cur_seq()).expect("the open journal entry is in flight")
    }

    fn entry_overhead(&self) -> i64 {
        (size_of::<Head<c::jset>>() / size_of::<u64>()) as i64 + i64::from(self.lock.entry_u64s_reserved.get())
    }

    #[cold]
    fn buf_to_text(&self, out: &mut Printbuf, g: &JournalGuard<'_>, buf: &c::journal_buf, seq: u64) {
        let s = self.reservations();
        let b = g.get_of(&buf.bits);

        prt!(out, "seq:\t{seq}\n");
        let mut indent = out.indent(2);
        let out = &mut *indent;

        if !b.write_started() {
            prt!(out, "refcount:\t{}\n", s.count(state_idx(seq)));
        }

        prt!(out, "write:\t");
        match g.get_of(&buf.write) {
            Some(w) => w.to_text(out),
            None    => prt!(out, "none\n"),
        }
        buf.data.to_text(g, out);

        let w = buf.wait_word().load(Relaxed);
        prt!(out, "waiters:\t{}\n", if w.is_null() {
            "none"
        } else if w == JOURNAL_BUF_NOT_IN_FLIGHT {
            "not in flight"
        } else if w == JOURNAL_BUF_NOFLUSH {
            "noflush"
        } else if w == JOURNAL_BUF_FLUSH_NO_WAIT {
            "flush no wait"
        } else if w == JOURNAL_BUF_FLUSH {
            "flush"
        } else {
            "(waiters)"
        });

        prt!(out, "flags:\t");
        for (set, name) in [(b.separate_flush(),             "separate_flush "),
                            (b.need_flush_to_write_buffer(), "need_flush_to_write_buffer "),
                            (b.write_started(),              "write_started "),
                            (b.write_allocated(),            "write_allocated "),
                            (b.write_done(),                 "write_done")] {
            if set {
                prt!(out, "{name}");
            }
        }
        out.newline();
    }

    #[cold]
    fn bufs_to_text(&self, out: &mut Printbuf, g: &JournalGuard<'_>) {
        let _f = MemallocFlags::noreclaim();

        if out.nr_tabstops() == 0 {
            out.tabstop_push(24);
        }

        for (seq, buf) in self.in_flight.locked(g).iter_indexed() {
            self.buf_to_text(out, g, buf, seq);
        }
        prt!(out, "last buf {}\n", if self.reservations().is_open() { "open" } else { "closed" });
    }

    /*
     * Detect stuck journal conditions and trigger shutdown. Technically the journal
     * can end up stuck for a variety of reasons, such as a blocked I/O, journal
     * reservation lockup, etc. Since this is a fatal error with potentially
     * unpredictable characteristics, we want to be fairly conservative before we
     * decide to shut things down.
     *
     * Consider the journal stuck when it appears full with no ability to commit
     * btree transactions, to discard journal buckets, nor acquire priority
     * (reserved watermark) reservation.
     */
    fn error_check_stuck(&self, error: BchError, flags: u32) -> bool {
        let fs = self.fs();
        let ret = !self.lock.can_discard.get() &&
            self.lock.seq_ondisk.get() == fs.journal_cur_seq() &&
            self.lock.cur_entry_error.get() != 0 &&
            (flags & c::BCH_WATERMARK_MASK) >= c::Watermark::reclaim as u32;

        if ret {
            let mut msg = LogMsg::new(fs);
            msg.set_suppressed(true); /* only print once, when we go ERO */

            prt!(&mut *msg, "Journal stuck! Have a pre-reservation but journal full (error {})", error.msg());

            self.to_text(&mut msg);

            fs.emergency_read_only(&mut msg);
        }

        ret
    }

    /*
     * Final processing when the last reference of a journal buffer has been
     * dropped. Drop the pin list reference acquired at journal entry open and write
     * the buffer, if requested.
     */
    fn buf_put_final(&self, pins: &PinsRead<'_>, g: &mut JournalGuard<'_>, seq: u64) {
        if pins.seq_put(seq) {
            self.update_last_seq(pins, g);
        }
        self.do_writes_locked(g);

        /*
         * Waiters include __bch2_next_write_buffer_flush_journal_buf(),
         * when quiescing an open journal entry journal_res_get_slowpath()
         * waiters blocked on journal_max_open — the refcount drop may have
         * enabled do_writes_locked() to advance seq_write_started above).
         */
        self.wake_async_wait();
    }

    fn buf_put_locked(&self, pins: &PinsRead<'_>, g: &mut JournalGuard<'_>, seq: u64) {
        let idx = state_idx(seq);
        if self.state_buf_put(idx).count(idx) == 0 {
            self.buf_put_final(pins, g, seq);
        }
    }

    #[inline]
    fn buf_put(&self, seq: u64) {
        let idx = state_idx(seq);
        let s = self.state_buf_put(idx);
        if s.count(idx) == 0 {
            self.buf_put_last(seq);
        } else if unlikely(s.cur_entry_offset() == c::JOURNAL_ENTRY_BLOCKED_VAL) {
            self.wake_async_wait();
        }
    }

    #[inline(never)]
    fn buf_put_last(&self, seq: u64) {
        let pins = self.pins();
        self.buf_put_final(&pins, &mut self.lock.lock(), seq);
    }

    /*
     * Fastpath access to the staging buffer (jset) for a held reservation. No
     * lock required: the reservation holds a count on the ring slot's state
     * index, so the slot still points at the data for res->seq until the
     * reservation is released.
     */
    fn res_entry(&self, res: &c::journal_res) -> *mut Head<c::jset_entry> {
        let data = self.lock.hot.ring_data[state_idx(res.seq) as usize].load(Relaxed);
        unsafe { data.byte_add(size_of::<Head<c::jset>>()).cast::<u64>().add(res.offset as usize).cast() }
    }

    /// Entries written into @res, from where it's at, using all of it: what @f
    /// doesn't write is empty entries.
    pub fn res_append_entries<R>(&self, res: &mut c::journal_res, f: impl FnOnce(&mut EntryWriter<'_>) -> R) -> R {
        ebug_assert!(res.ref_, "entries into a journal reservation not held");

        let buf = unsafe { core::slice::from_raw_parts_mut(self.res_entry(res).cast::<u64>(), res.u64s.into()) };
        let mut w = EntryWriter::new(buf);
        let ret = f(&mut w);
        w.pad();

        res.offset += u32::from(res.u64s);
        res.u64s = 0;
        ret
    }

    /*
     * This function releases the journal write structure so other threads can
     * then proceed to add their keys as well.
     */
    #[inline]
    pub fn res_put(&self, res: &mut c::journal_res) {
        if !res.ref_ {
            return;
        }

        #[cfg(CONFIG_DEBUG_LOCK_ALLOC)]
        self.res_map.release(0);

        if unlikely(res.u64s != 0) {
            self.res_pad(res);
        }

        self.buf_put(res.seq);

        res.ref_ = false;
    }

    /// A reservation put with room it didn't write: filled with empty entries.
    #[cold]
    #[inline(never)]
    fn res_pad(&self, res: &mut c::journal_res) {
        self.res_append_entries(res, |_| ());
    }

    #[inline(always)]
    fn res_get_fast(&self, res: &mut c::journal_res, flags: u32) -> bool {
        let u64s = u32::from(res.u64s);
        let mut check_ok = false;
        let Ok(old) = self.reservations_update(|old| {
            let mut new = old;

            /*
             * Check if there is still room in the current journal
             * entry, smp_rmb() guarantees that reads from reservations.counter
             * occur before accessing cur_entry_u64s:
             */
            fence(Acquire);
            if new.cur_entry_offset() + u64s > self.lock.hot.cur_entry_u64s.get() {
                return None;
            }

            ebug_assert!(new.count(new.idx()) != 0, "journal entry open with no ref on its buf");

            if (flags & c::BCH_WATERMARK_MASK) < self.lock.hot.watermark.get() {
                return None;
            }

            new.set_cur_entry_offset(new.cur_entry_offset() + u64s);

            /*
             * If the refcount would overflow, we have to wait:
             * XXX - tracepoint this:
             */
            if !new.inc() {
                return None;
            }

            if flags & c::JOURNAL_RES_GET_CHECK != 0 {
                check_ok = true;
                return None;
            }

            Some(new)
        }) else {
            return check_ok;
        };

        res.ref_ = true;
        res.offset = old.cur_entry_offset();
        let seq = self.lock.hot.res_seq.load(Relaxed) as u64;
        res.seq = seq - (seq.wrapping_sub(u64::from(old.idx())) & u64::from(c::JOURNAL_STATE_BUF_MASK));
        res.has_overwrites = self.lock.hot.ring_has_overwrites[state_idx(res.seq) as usize].get();
        true
    }

    #[inline(always)]
    pub fn res_get(&self, res: &mut c::journal_res, u64s: u32, flags: u32, trans: Option<&TransAttempt<'_, '_>>)
        -> Result<(), BchError>
    {
        self.res_get_waiting(res, u64s, flags, |ev, cond, timeout| match trans {
            Some(trans) => trans.wait_until_timeout(ev, || cond().then_some(()), timeout).is_some(),
            None => ev.wait_until(Sleep::UNINTERRUPTIBLE.timeout(timeout as c_ulong), || cond().then_some(())).is_ok(),
        })
    }

    /// res_get(), blocking with @wait: wait_until_timeout() as the caller's
    /// transaction does it.
    #[inline(always)]
    pub fn res_get_waiting(&self, res: &mut c::journal_res, u64s: u32, flags: u32,
                           mut wait: impl FnMut(&EventCount, &mut dyn FnMut() -> bool, c_long) -> bool)
        -> Result<(), BchError>
    {
        ebug_assert!(!res.ref_, "journal reservation taken while holding one");
        ebug_assert!(self.flags.get(Journal::running), "journal reservation before the journal is running");
        #[cfg(CONFIG_BCACHEFS_DEBUG)]
        {
            let t = self.stop_thread.load(Relaxed);
            assert!(t.is_null() || t == crate::os::sched::current() || self.halted(),
                    "journal reservation after the journal stopped");
        }

        res.u64s = u64s as u16;

        if !self.res_get_fast(res, flags) {
            self.res_get_slowpath(res, flags, &mut wait)?;
        }

        if flags & c::JOURNAL_RES_GET_CHECK == 0 {
            #[cfg(CONFIG_DEBUG_LOCK_ALLOC)]
            self.res_map.acquire(flags & c::JOURNAL_RES_GET_NONBLOCK != 0, true, 0);
            ebug_assert!(res.ref_, "journal reservation got, with no ref");
            assert!(res.seq != 0, "journal reservation got, with no seq");
        }
        Ok(())
    }

    /*
     * Close the currently-open journal entry. No recursion — callers that want
     * the close-then-maybe-open behavior of the prior __journal_entry_close go
     * through cycle_locked() instead.
     */
    fn entry_close(&self, g: &mut JournalGuard<'_>, how: Close<'_>) {
        let fs = self.fs();
        let closed_val = match how {
            Close::Closed(_) => c::JOURNAL_ENTRY_CLOSED_VAL,
            Close::Error => c::JOURNAL_ENTRY_ERROR_VAL,
        };

        let Ok(old) = self.reservations_update(|old| {
            let mut new = old;
            new.set_cur_entry_offset(closed_val);

            (old.cur_entry_offset() != c::JOURNAL_ENTRY_ERROR_VAL &&
             old.cur_entry_offset() != closed_val).then_some(new)
        }) else {
            return;
        };

        if !old.is_open() {
            return;
        }

        let seq = fs.journal_cur_seq();

        let offset = if old.cur_entry_offset() == c::JOURNAL_ENTRY_BLOCKED_VAL {
            g.cur_entry_offset_if_blocked
        } else {
            old.cur_entry_offset()
        };

        let buf = self.in_flight.locked(g).get(seq).expect("the open journal entry is in flight");
        /* Close out old buffer: */
        let h = buf.data.header(g);
        h.set_u64s(offset as usize);
        let entry_bytes = size_of::<Head<c::jset>>() + h.u64s() * size_of::<u64>();

        if let Close::Closed(pins) = how {
            let bytes = entry_bytes.next_power_of_two();

            pins.seq_set_bytes(seq, bytes as u32);
            g.dirty_entry_bytes += bytes;
        }

        fs.event_trace(events::journal_entry_close, |out| {
            let _f = MemallocFlags::noreclaim();

            prt!(out, "entry size: {}\n", HumanReadable(entry_bytes as u64));
            let _g = MemallocFlags::for_gfp(flags::NOWAIT);
            let _ = prt_current_backtrace(out, 1);
        });

        let u64s_reserved = *g.get_of(&buf.u64s_reserved);
        let sectors = ((entry_bytes + u64s_reserved as usize * size_of::<u64>())
            .next_multiple_of(512 << fs.block_bits()) >> 9) as u32;
        if unlikely(sectors > *g.get_of(&buf.sectors)) {
            let _f = MemallocFlags::noreclaim();
            let mut msg = LogMsg::new(fs);
            prt!(&mut *msg, "journal entry overran reserved space: {sectors} > {}\n", g.get_of(&buf.sectors));
            prt!(&mut *msg, "buf u64s {offset} u64s reserved {u64s_reserved} cur_entry_u64s {} block_bits {}\n",
                 self.lock.hot.cur_entry_u64s.get(),
                 fs.block_bits());
            fs.emergency_read_only_locked(&mut msg, g);
            return;
        }

        *g.get_mut_of(&buf.sectors) = sectors;

        /*
         * We have to set last_seq here, _before_ opening a new journal entry:
         *
         * A threads may replace an old pin with a new pin on their current
         * journal reservation - the expectation being that the journal will
         * contain either what the old pin protected or what the new pin
         * protects.
         *
         * After the old pin is dropped j->last_seq won't include the old
         * pin, so we can only write the updated last_seq on the entry that
         * contains whatever the new pin protects.
         *
         * Restated, we can _not_ update last_seq for a given entry if there
         * could be a newer entry open with reservations/pins that have been
         * taken against it.
         *
         * Hence, we want update/set last_seq on the current journal entry right
         * before we open a new one:
         */
        let last_seq = self.lock.last_seq.get();
        *g.get_mut_of(&buf.last_seq) = last_seq;
        let h = buf.data.header(g);
        h.last_seq.set(last_seq);
        assert!(last_seq <= h.seq.get(), "journal entry {seq} closed with last_seq {last_seq}, past it");

        /*
         * bch2_journal_halt may or may not be called with pin_resize_lock held,
         * and we don't have a recursive version of this lock.
         *
         * So, on emergency ERO skip the journal_buf_put - this leaks a ref, but
         * it's for updating last_seq, so it's ok:
         */
        match how {
            Close::Closed(pins) => {
                self.buf_put_locked(pins, g, seq);
                self.space_available(g);
            }
            Close::Error => {
                let idx = state_idx(seq);
                if self.state_buf_put(idx).count(idx) == 0 {
                    self.do_writes_locked(g);
                }
            }
        }
    }

    /*
     * Open a new journal entry. No recursion — the post-open "must close
     * immediately" check lives in cycle_locked() now.
     */
    fn entry_open(&self, pins: &PinsRead<'_>, g: &mut JournalGuard<'_>) -> Result<(), BchError> {
        let fs = self.fs();

        if self.reservations().is_open() {
            return Ok(());
        }

        if self.lock.blocked.get() != 0 {
            return fs.throw(Errcode::journal_blocked);
        }

        let cur_entry_error = self.lock.cur_entry_error.get();
        if cur_entry_error != 0 {
            return Err(BchError::from_raw(cur_entry_error));
        }

        self.error()?;

        if self.pin_range().used() >= self.pin_range().pin_size.get() {
            return fs.throw(Errcode::journal_pin_full);
        }

        /*
         * Grow the fifo when it's more than half full, before the low_on_pin
         * watermark (3/4 full) throttles - up to JOURNAL_PIN_MAX, past which
         * low_on_pin is all there is; or shrink an over-size one, from a mount
         * that found more than that dirty, once reclaim has caught up.
         * queue_work is idempotent — subsequent triggers while a resize is
         * pending coalesce. Off the open path because some callers hit it with
         * btree locks held (JOURNAL_RES_GET_NONBLOCK) and can't tolerate the
         * alloc/sleep.
         */
        if self.pin_fifo_want_resize() {
            self.pin_resize_work.queue(&self.wq);
        }

        /*
         * Need room in in_flight and a pre-allocated data buffer;
         * the data buffer is topped up by journal_buf_prealloc() outside of
         * j->lock.
         *
         * Require two free slots, not one: the entry opened below takes one,
         * and we must keep >= 1 free so a JOURNAL_BUF_NOT_IN_FLIGHT slot always
         * exists past the open entry. __bch2_journal_flush_seq_async()'s forward
         * walk relies on that sentinel to terminate — a full in_flight would wrap
         * back onto a live entry.
         */
        if self.in_flight.locked(g).free() < 2 {
            return fs.throw(Errcode::journal_max_in_flight);
        }

        let cur_seq = fs.journal_cur_seq();

        if self.reservations().count(state_idx(cur_seq + 1)) != 0 {
            return fs.throw(Errcode::journal_max_open);
        }

        if unlikely(cur_seq >= c::JOURNAL_SEQ_MAX) {
            let _f = MemallocFlags::noreclaim();
            let mut msg = LogMsg::new(fs);
            prt!(&mut *msg, "cannot start: journal seq overflow\n");
            fs.emergency_read_only_locked(&mut msg, g);
            return fs.throw(Errcode::journal_shutdown);
        }

        if unlikely(fs.journal_seq_is_blacklisted(cur_seq + 1, false)) {
            let _f = MemallocFlags::noreclaim();
            let mut msg = LogMsg::new(fs);
            prt!(&mut *msg, "attempting to open blacklisted journal seq {}\n", cur_seq + 1);
            fs.emergency_read_only_locked(&mut msg, g);
            return fs.throw(Errcode::journal_shutdown);
        }

        let Some(free_size) = g.free_buf.as_ref().map(c::JsetBuf::size) else {
            return fs.throw(Errcode::journal_buf_enomem);
        };

        assert!(g.cur_entry_sectors != 0, "opening a journal entry with no room for one on disk");

        let sectors = g.cur_entry_sectors.min(free_size >> 9);

        let u64s = (i64::from(sectors << 9) / size_of::<u64>() as i64 - self.entry_overhead())
            .clamp(0, i64::from(c::JOURNAL_ENTRY_CLOSED_VAL) - 1);

        if u64s <= g.early_journal_entries.len() as i64 {
            return fs.throw(Errcode::journal_full);
        }

        let was_empty = self.pin_range().used() == 0;

        /*
         * The fifo_push() needs to happen at the same time as j->seq is
         * incremented for j->last_seq to be calculated correctly
         */
        let seq = (self.lock.seq.fetch_add(1, SeqCst) + 1) as u64;
        self.lock.hot.res_seq.store(seq as i64, Relaxed);
        pins.push(seq);

        let has_overwrites = fs.opts().journal_transaction_names.get() != 0;

        /* Claim the pre-allocated data buffer */
        let mut data = g.free_buf.take().expect("j->free_buf, checked above");

        data.bytes_mut()[..size_of::<Head<c::jset>>()].fill(0);
        data.head_mut().seq.set(seq);
        data.head_mut().set_u64s(0);

        let early = mem::take(&mut g.early_journal_entries);
        let data_ptr = data.as_ptr();

        /*
         * The new seq's slot, reset in place: all of it but the wait word,
         * which flushers read without j->lock.
         */
        let disk_sectors = g.cur_entry_sectors;
        let buf = self.in_flight.locked(g).get(seq).expect("the entry being opened, in flight");
        buf.data.put(g, data);
        let write = g.free_write.take();
        *g.get_mut_of(&buf.write) = write;
        *g.get_mut_of(&buf.last_seq) = 0;
        *g.get_mut_of(&buf.sectors) = sectors;
        *g.get_mut_of(&buf.disk_sectors) = disk_sectors;
        *g.get_mut_of(&buf.u64s_reserved) = self.lock.entry_u64s_reserved.get();
        *g.get_mut_of(&buf.bits) = c::journal_buf_bits::new()
            .with_need_flush_to_write_buffer(true)
            .with_has_overwrites(has_overwrites);

        *g.get_mut_of(&buf.key) = c::JournalKey::empty();

        /* if filesystem is clean, the first journal write must be a flush */
        if fs.sb.clean.get() != 0 {
            self.flags.set(Journal::need_flush_write, true);
        }

        /*
         * last_seq_ondisk == seq means the journal was clean before this entry
         * (the on-disk last_seq has caught up to seq - nothing older is pinned):
         * this is the clean->dirty transition. Stamp it FLUSH_NO_WAIT so it's
         * forced to a flush write (its completion marks the fs dirty), but
         * flushers don't wait on it - they'd be signalled before that mark lands.
         * Leave flush_wait alone; its waiters migrate onto the next entry, which
         * completes only after this entry's superblock write.
         *
         * This doesn't re-arm on an empty forced flush the way a "journal was
         * empty" test would: the entry pins itself while in flight, so its
         * recorded last_seq is its own seq, and last_seq_ondisk only catches back
         * up to a new seq once the journal is genuinely clean again.
         */
        let new_first = if self.lock.last_seq_ondisk.get() == seq {
            JOURNAL_BUF_FLUSH_NO_WAIT
        } else {
            self.flush_wait.swap(ptr::null_mut(), SeqCst)
        };

        let was = buf.wait_word().swap(new_first, SeqCst);
        assert!(was == JOURNAL_BUF_NOT_IN_FLIGHT, "journal seq {seq} opened on a slot still in flight");

        /*
         * Publish to the reservation fastpath ring slot. Must happen before
         * the atomic reservation-state cmpxchg below so that reservations on
         * the new state idx see the ring slot populated.
         */
        let idx = state_idx(seq);
        self.lock.hot.ring_data[idx as usize].store(data_ptr, Relaxed);
        g.published().hot.ring_has_overwrites[idx as usize].set(has_overwrites);

        /*
         * What was journalled before the journal started, as the body's first
         * reservation: no other is taken till the entry's opened below.
         */
        let early_res = c::journal_res { seq, ..Default::default() };
        unsafe { self.res_entry(&early_res).cast::<u64>().copy_from_nonoverlapping(early.as_ptr(), early.len()) };

        /*
         * Must be set before marking the journal entry as open:
         */
        g.published().hot.cur_entry_u64s.set(u64s as u32);

        let _ = self.reservations_update(|old| {
            assert!(old.cur_entry_offset() != c::JOURNAL_ENTRY_ERROR_VAL, "journal entry opened on a halted journal");

            let mut new = old;
            new.set_idx(old.idx() + 1);
            assert!(new.count(new.idx()) == 0, "journal seq {seq} opened on a ring slot with refs");
            assert!(new.idx() == idx, "journal seq {seq} opened on ring slot {}", new.idx());

            new.inc();

            /* Handle any already added entries */
            new.set_cur_entry_offset(early.len() as u32);
            Some(new)
        });

        self.wake_async_wait();

        if was_empty {
            self.reclaim_wake();
        }

        drop(early);

        self.write_work.queue(Own, msecs_to_jiffies(fs.opts().journal_flush_delay.get().into()));

        Ok(())
    }

    /// Whether anything's on flush_wait - a hint, racing its adds and wakes.
    fn flush_waiting(&self) -> bool {
        !self.flush_wait.load(Relaxed).is_null()
    }

    fn has_flush_waiters(&self, g: &JournalGuard<'_>) -> bool {
        self.flush_waiting() ||
            (self.reservations().is_open() && self.cur_buf(g).must_flush())
    }

    fn should_cycle_for_flush_waiters(&self, g: &JournalGuard<'_>) -> bool {
        if self.reservations().is_open() {
            self.has_flush_waiters(g) && self.in_flight.locked(g).len() <= 1
        } else {
            self.flush_waiting()
        }
    }

    fn should_close(&self, g: &JournalGuard<'_>, flags: c::JournalCycleFlags) -> bool {
        if !self.reservations().is_open() {
            return false;
        }

        if flags.contains(Cycle::must_open) {
            return true;
        }

        if !flags.contains(Cycle::must_close) && !self.cur_buf(g).must_flush() {
            return false;
        }

        flags.contains(Cycle::force_close) || self.in_flight.locked(g).len() <= 1
    }

    fn should_open(&self, flags: c::JournalCycleFlags) -> bool {
        !self.reservations().is_open() &&
            (flags.contains(Cycle::must_open) || self.flush_waiting())
    }

    /*
     * Cycle the journal entry state between closed and open as needed.
     *
     * This replaces the prior mutually-recursive __journal_entry_close ↔
     * journal_entry_open. The two transitions have natural "and now maybe
     * do the other" tails:
     *   - close: a flush_wait waiter wants the new entry to absorb it
     *   - open:  the just-opened entry must flush immediately (must_flush)
     * Folding both tails into a loop here makes the state machine explicit.
     *
     * @flags: must_close closes the current entry, once older writes
     *         have drained, even if no flush waiter currently requires it;
     *         force_close skips the older-write throttle for callers
     *         that must close immediately; must_open opens a fresh
     *         entry for a following reservation.
     *
     * Returns: error from open if open was attempted and failed, else 0.
     */
    pub(crate) fn cycle_locked(&self, pins: &PinsRead<'_>, g: &mut JournalGuard<'_>,
                               mut flags: c::JournalCycleFlags) -> Result<(), BchError> {
        loop {
            if flags.is_empty() && !self.should_cycle_for_flush_waiters(g) {
                return Ok(());
            }

            if self.should_close(g, flags) {
                self.entry_close(g, Close::Closed(pins));
            }

            flags.remove(Cycle::must_close | Cycle::force_close);

            if !self.should_open(flags) {
                return Ok(());
            }

            self.entry_open(pins, g)?;

            flags.remove(Cycle::must_open);
        }
    }

    pub fn cycle(&self, flags: c::JournalCycleFlags) {
        let pins = self.pins();
        let _ = self.cycle_locked(&pins, &mut self.lock.lock(), flags);
    }

    pub(crate) fn halt_locked(&self, g: &mut JournalGuard<'_>) {
        self.entry_close(g, Close::Error);
        if self.lock.err_seq.get() == 0 {
            g.published().err_seq.set(self.fs().journal_cur_seq());
        }
        self.wake_async_wait();

        /*
         * Wake flush waiters who'd otherwise be parked indefinitely once
         * we enter error state: no future write_done will fire, so the
         * usual per-write wake-ups (and reclaim-side wake-ups) won't reach
         * them. They re-check on wake, see the journal is shutting down,
         * and return the appropriate error.
         */
        if let Some(waiters) = Waiters::take(&self.flush_wait, ptr::null_mut()) {
            waiters.wake();
        }
        self.reclaim_flush_wait.wake();
    }

    pub fn halt(&self) {
        self.halt_locked(&mut self.lock.lock())
    }

    fn quiesced(&self) -> bool {
        let pins = self.pins();
        let mut g = self.lock.lock();
        let ret = self.lock.seq.load(Relaxed) as u64 == self.lock.seq_ondisk.get();

        if !ret {
            let _ = self.cycle_locked(&pins, &mut g, Cycle::must_close);
        }
        ret
    }

    fn quiesce(&self) {
        let Ok(()) = self.async_wait.wait_until(Sleep::UNINTERRUPTIBLE, || self.quiesced().then_some(()));
    }

    /*
     * Shutdown-only quiesce: bch2_journal_quiesce() waits for writes to have
     * landed on disk, but journal_write_done() still has bookkeeping to do after
     * the write completes - notably bch2_journal_update_last_seq_ondisk() and
     * replicas_refs_put(), which is what triggers the final mark_clean via
     * __replicas_entry_kill() + bch2_write_super(). Wait for all of that to
     * drain so the fs ends up marked clean.
     *
     * On journal error, flushed_seq_ondisk stops advancing (the update path in
     * journal_write_done is gated on !j->err_seq), so fall back to the regular
     * quiesce condition (seq == seq_ondisk), which always terminates since
     * seq_ondisk is updated unconditionally for every completed write. Handles
     * the race where the journal transitions to error state mid-wait.
     */
    fn shutdown_quiesced(&self) -> bool {
        let pins = self.pins();
        let mut g = self.lock.lock();
        let seq = self.lock.seq.load(Relaxed) as u64;
        let ret = (if self.error().is_err() {
            seq == self.lock.seq_ondisk.get()
        } else {
            seq == self.lock.flushed_seq_ondisk.get()
        }) && !self.flush_waiting();

        if !ret {
            let _ = self.cycle_locked(&pins, &mut g, Cycle::must_close);
        }
        ret
    }

    pub fn shutdown_quiesce(&self) {
        let Ok(()) = self.async_wait.wait_until(Sleep::UNINTERRUPTIBLE, || self.shutdown_quiesced().then_some(()));
    }

    fn buf_prealloc<'a>(&'a self, mut g: JournalGuard<'a>) -> JournalGuard<'a> {
        if g.free_buf.as_ref().is_some_and(|b| b.size() >= self.lock.buf_size_want.get()) {
            return g;
        }

        let buf_size = self.lock.buf_size_want.get();

        drop(g);
        let new = c::jset_buf_alloc(buf_size, flags::NOIO);
        g = self.lock.lock();

        let Ok((buf, write)) = new else {
            return g;
        };

        let old = (g.free_buf.replace(buf), g.free_write.replace(write));

        if unlikely(old.0.is_some()) {
            drop(g);
            /* kvfree can sleep */
            drop(old);
            g = self.lock.lock();
        }
        g
    }

    /*
     * Close any blocked-state stats that the slowpath wait may have opened. Called
     * on every __journal_res_get() success so durations reflect "for how long was
     * a writer blocked on this condition". Cheap when nothing was open
     * (track_event_change is a single compare-against-prior-state).
     *
     * blocked_journal_blocked and blocked_journal_max_in_flight are *not* closed
     * here: those track the actual system condition (j->blocked / journal write
     * in-flight count) and have their own close sites tied to the condition
     * clearing.
     */
    fn res_unblocked(&self) {
        use c::TimeStat as Time;

        let fs = self.fs();
        for t in [Time::blocked_journal_max_open,
                  Time::blocked_journal_full,
                  Time::blocked_journal_pin_full,
                  Time::blocked_journal_buf_enomem,
                  Time::blocked_journal_stuck] {
            fs.times.get(t as usize).track_event_change(false);
        }
    }

    fn __res_get(&self, res: &mut c::journal_res, flags: u32) -> Result<(), BchError> {
        use c::TimeStat as Time;

        let fs = self.fs();

        let ret = loop {
            if self.res_get_fast(res, flags) {
                break Ok(());
            }

            self.error()?;

            if self.lock.blocked.get() != 0 {
                return fs.throw(Errcode::journal_blocked);
            }

            if (flags & c::BCH_WATERMARK_MASK) < self.lock.hot.watermark.get() {
                break fs.throw(Errcode::journal_full);
            }

            let pins = self.pins();
            let mut g = self.buf_prealloc(self.lock.lock());

            /*
             * Recheck after taking the lock, so we don't race with another thread
             * that just opened a new entry and cycle the journal unnecessarily
             */
            if self.res_get_fast(res, flags) {
                break Ok(());
            }

            /*
             * If we couldn't get a reservation because the current buf filled up,
             * and we had room for a bigger entry on disk, signal that we want to
             * realloc the journal bufs:
             */
            if self.reservations().is_open() {
                let buf = self.cur_buf(&g);
                let size = buf.data.size(&g);
                if size >> 9 < *g.get_of(&buf.disk_sectors) && size < c::JOURNAL_ENTRY_SIZE_MAX {
                    let want = self.lock.buf_size_want.get().max(size << 1);
                    g.published().buf_size_want.set(want);
                }
            }

            if let Err(e) = self.cycle_locked(&pins, &mut g, Cycle::must_open) {
                break Err(e);
            }
        };

        let mut err = match ret {
            Ok(()) => {
                self.res_unblocked();
                return Ok(());
            }
            Err(e) => e,
        };

        if self.error_check_stuck(err, flags) {
            err = fs.err(Errcode::journal_stuck);
        }

        let blocked = [(Errcode::journal_blocked,       Time::blocked_journal_blocked),
                       (Errcode::journal_max_in_flight, Time::blocked_journal_max_in_flight),
                       (Errcode::journal_max_open,      Time::blocked_journal_max_open),
                       (Errcode::journal_full,          Time::blocked_journal_full),
                       (Errcode::journal_pin_full,      Time::blocked_journal_pin_full),
                       (Errcode::journal_buf_enomem,    Time::blocked_journal_buf_enomem),
                       (Errcode::journal_stuck,         Time::blocked_journal_stuck)]
            .into_iter().find(|&(e, _)| err == e.into());
        if let Some((_, t)) = blocked {
            fs.times.get(t as usize).track_event_change(true);
        }

        if err.matches(Errcode::operation_blocked) {
            fs.event_inc_trace(events::journal_res_get_blocked, |out| {
                prt!(out, "{}\n", err.msg());
                out.make_room(4096);

                let g = self.lock.lock();
                prt!(out, "seq {}\n", fs.journal_cur_seq());
                self.bufs_to_text(out, &g);
            });
        }

        /*
         * Journal is full - can't rely on reclaim from work item due to
         * freezing:
         */
        if (err == Errcode::journal_full.into() || err == Errcode::journal_pin_full.into()) &&
            flags & c::JOURNAL_RES_GET_NONBLOCK == 0 &&
            (self.flags.get(Journal::low_on_space) || self.flags.get(Journal::low_on_pin)) {
            if let Some(mut reclaim) = self.reclaim_lock.try_lock() {
                let _ = self.reclaim(&mut reclaim);
            }
        }

        Err(err)
    }

    /*
     * Essentially the entry function to the journaling code. When bcachefs is doing
     * a btree insert, it calls this function to get the current journal write.
     * Journal write is the structure used set up journal writes. The calling
     * function will then add its keys to the structure, queuing them for the next
     * write.
     *
     * To ensure forward progress, the current task must not be holding any
     * btree node write locks.
     */
    #[inline(never)]
    fn res_get_slowpath(&self, res: &mut c::journal_res, flags: u32,
                        wait_on: &mut dyn FnMut(&EventCount, &mut dyn FnMut() -> bool, c_long) -> bool)
        -> Result<(), BchError>
    {
        if flags & c::JOURNAL_RES_GET_NONBLOCK != 0 {
            return self.__res_get(res, flags);
        }

        let fs = self.fs();
        /*
         * Over every online device, not just the journal's: a flushing commit
         * preflushes every rw member (journal_write_preflush()), so a member
         * that carries no journal at all still paces the wait. Sizing this from
         * the journal devices alone reports "stuck" routinely on an array whose
         * slowest member is not a journal member.
         */
        let online = c::bch_devs_mask { d: fs.devs_online.load() };
        let total_wait = (fs.dev_latency_max(&online, WRITE as c_int) * 2).max(HZ * 10) as c_long;

        let ret = Cell::new(Ok(()));
        let mut got = || {
            ret.set(self.__res_get(res, flags));
            !ret.get().is_err_and(|e| e.matches(Errcode::operation_blocked))
        };
        let mut wait = |timeout| wait_on(&self.async_wait, &mut got, timeout);

        if wait(total_wait) {
            return ret.get();
        }

        /* every waiter times out together: one report per time we're stuck */
        if !self.flags.swap(Journal::stuck_reported, true) {
            let mut msg = LogMsg::new(fs);
            prt!(&mut *msg, "Journal stuck? Waited for {}s, err {}\n",
                 total_wait / HZ as c_long, ret.get().err().map_or("", |e| e.msg()));
            self.to_text(&mut msg);
        }

        wait(c_long::MAX);
        self.flags.set(Journal::stuck_reported, false);
        ret.get()
    }

    /* journal_entry_res: */

    pub fn entry_res_resize(&self, res: &c::journal_entry_res, new_u64s: u32) {
        let pins = self.pins();
        let mut g = self.lock.lock();
        let d = new_u64s as i32 - res.u64s.get() as i32;

        let reserved = self.lock.entry_u64s_reserved.get();
        g.published().entry_u64s_reserved.set(reserved.wrapping_add_signed(d));
        res.u64s.set(new_u64s);

        if d <= 0 {
            return;
        }

        let cur_entry_u64s = (self.lock.hot.cur_entry_u64s.get() as i32 - d).max(0) as u32;
        g.published().hot.cur_entry_u64s.set(cur_entry_u64s);
        let state = self.reservations();

        if state.cur_entry_offset() >= c::JOURNAL_ENTRY_CLOSED_VAL {
            return;
        }

        if state.cur_entry_offset() > cur_entry_u64s {
            g.published().hot.cur_entry_u64s.set(cur_entry_u64s + d as u32);
            /*
             * Not enough room in current journal entry, have to flush it:
             */
            let _ = self.cycle_locked(&pins, &mut g, Cycle::must_close | Cycle::force_close);
        } else {
            let buf = self.cur_buf(&g);
            *g.get_mut_of(&buf.u64s_reserved) += d as u32;
        }
    }

    /* journal flushing: */

    /// @e, waiting for @seq's flush - which it asks for - till it's
    /// completed: on a buf's word or flush_wait, the one returned.
    pub fn __flush_seq_async(&self, mut seq: u64, e: &FlushEntry) -> &AtomicPtr<FlushEntry> {
        let mut wait = None;

        /*
         * Walk forward to the first live entry (open or in flight) and attach e
         * to it; its write completion (or halt) wakes us.
         *
         * A slot reads as JOURNAL_BUF_NOT_IN_FLIGHT when no entry occupies it: the
         * journal is empty from here on, so there is nothing in flight to ride to
         * disk. Skipping past a non-waitable buf is only for NOFLUSH — a
         * written-but-not-flushed entry — where we want a newer flushable seq.
         */
        loop {
            let buf = self.in_flight.buf(seq);
            match buf.wait(e) {
                BufWait::NotWaitable => seq += 1,
                BufWait::NotInFlight => break,
                BufWait::Waiting { first } => {
                    let w = buf.wait_word();
                    /* If the waitlist already had waiters we can skip the
                     * barrier and close check */
                    if !first {
                        return w;
                    }
                    wait = Some(w);
                    break;
                }
            }
        }

        /*
         * Nothing live to attach to: park on flush_wait. Its non-emptiness is the
         * trigger that makes bch2_journal_entry_close()/journal_entry_open() open a
         * fresh entry and flush it, with the __journal_entry_open() xchg migrating
         * us onto that buf. This is the only path that flushes an idle or
         * noflush-tailed journal; without it fsync of such a seq returns before the
         * data is durable.
         */
        let wait = wait.unwrap_or_else(|| {
            e.push(&self.flush_wait);
            &self.flush_wait
        });

        fence(SeqCst);

        let cycle = if self.reservations().is_open() {
            self.lock.seq_ondisk.get() + 1 == self.fs().journal_cur_seq()
        } else {
            self.flush_waiting()
        };
        if cycle {
            self.cycle(Cycle::empty());
        }

        self.fs().event_inc_trace(events::journal_flush, |out| prt!(out, "seq {seq}"));

        wait
    }

    /*
     * Wait for a journal entry to be written: Ok(true) if @seq has already been
     * flushed, Ok(false) if it's being flushed, journal_flush_err if @seq will
     * never be flushed.
     *
     * Like bch2_journal_wait_on_seq, except that it triggers a write immediately if
     * necessary
     */
    /// @attach puts the waiter on the flush of the seq it's to wait for -
    /// dropped uncalled, if it's not to wait: Ok(true), or an error.
    fn flush_seq_async(&self, mut seq: u64,
                       attach: impl for<'j> FnOnce(&'j Self, u64) -> &'j AtomicPtr<FlushEntry>)
        -> Result<bool, BchError>
    {
        let fs = self.fs();
        let flushed_seq_ondisk = self.lock.flushed_seq_ondisk.get();
        let cur_seq = fs.journal_cur_seq();

        if seq <= flushed_seq_ondisk {
            return Ok(true);
        }

        if warn_once!(seq > cur_seq, fs, "requested to flush journal seq {seq}, but currently at {cur_seq}") {
            return Ok(false);
        }

        /*
         * In error state, any seq that wasn't already durable before the
         * error fired won't ever flush — write_done's flushed_seq_ondisk
         * update is gated on !err_seq, so seqs in flight at the moment of
         * error are stranded with seq_ondisk advanced but
         * flushed_seq_ondisk not. Return -EIO for those instead of
         * waiting forever; fsync gets the error and unblocks.
         */
        if self.lock.err_seq.get() != 0 && seq > self.lock.flushed_seq_ondisk.get() {
            return fs.throw(Errcode::journal_flush_err);
        }

        /* if seq was written, but not flushed - flush a newer one instead */
        seq = seq.max(self.lock.seq_ondisk.get() + 1);

        self.flushing_seq.fetch_max(seq as i64, SeqCst);

        let wait = attach(self, seq);

        /*
         * Memory barrier between wait and checking for journal error is in
         * __bch2_journal_flush_seq_async(); bch2_journal_res_flush() doesn't
         * require this check because it has a journal res
         */
        if let Err(e) = self.error() {
            flush_seq_err_wake(wait);
            return Err(e);
        }
        Ok(false)
    }

    pub fn flush_seq(&self, seq: u64, _task_state: c_uint) -> Result<(), BchError> {
        let fs = self.fs();
        let start_time = local_clock();

        const FLUSHED: u32 = 1 << 0;
        let mut sel = core::pin::pin!(None);
        let (mut wait, [flushed, timed_out]) = Select::new(sel.as_mut(), waiter::Sync::current());
        let mut slot = core::pin::pin!(None);
        let flush = FlushWait::chained(slot.as_mut(), waiter::Sync::current(), flushed);
        let ret = self.flush_seq_async(seq, |j, seq| flush.attach(j, seq));

        /*
         * Don't report stuck until we've waited longer than a flush could
         * legitimately take: five times the longest write latency we've seen,
         * or 10s, whichever is greater.
         *
         * Deliberately a larger multiplier than res_get_slowpath()'s
         * 2x, because this waits on more than one IO: a flushing commit issues a
         * preflush to every rw member, then the journal write, then waits for
         * that to complete. Those serialize, so one device's worst-case latency
         * is a floor on the total, not the total.
         *
         * Over every online device, for the same reason: the slowest member
         * bounds this even when it holds no journal.
         */
        let online = c::bch_devs_mask { d: fs.devs_online.load() };
        let total_wait = (fs.dev_latency_max(&online, WRITE as c_int) * 5).max(HZ * 10);

        if slot.as_ref().as_pin_ref().expect("just made").armed() {
            pin_init::stack_pin_init!(let _timer = Timer::new(timed_out, total_wait));
            if wait.wait_sync(Sleep::UNINTERRUPTIBLE) & FLUSHED == 0 {
                let mut msg = LogMsg::new(fs);
                prt!(&mut *msg, "bch2_journal_flush_seq stuck? Waited {}s for seq {seq}\n", total_wait / HZ);
                self.to_text(&mut msg);
            }
        }
        // the flush's own wait - completed after the Select's: so all of it's done
        slot.as_mut().as_pin_mut().expect("just made").completion().wait_sync(Sleep::UNINTERRUPTIBLE);

        /* Don't update time_stats when @seq is already flushed: */
        if ret == Ok(false) {
            fs.times.get(c::TimeStat::journal_flush_seq as usize).update(start_time);
        }

        if self.lock.err_seq.get() != 0 && seq > self.lock.flushed_seq_ondisk.get() {
            fs.throw(Errcode::journal_flush_err)
        } else {
            Ok(())
        }
    }

    /*
     * If there is an open journal entry, or a journal still being written, write
     * it - waited for by no one
     */
    pub fn flush_async(&self) {
        let _ = self.flush_seq_async(self.lock.seq.load(Relaxed) as u64,
                                     |j, seq| j.__flush_seq_async(seq, FlushEntry::detached()));
    }

    /// Everything committed before the call is on disk when it returns
    pub fn flush(&self) -> Result<(), BchError> {
        self.flush_seq(self.lock.seq.load(Relaxed) as u64, c::TASK_UNINTERRUPTIBLE)
    }

    /*
     * Ask the journal not to issue any flushes in the range [start, end) - false
     * if it can't: one's been flushed
     */
    pub fn noflush_seq(&self, start: u64, end: u64) -> bool {
        let fs = self.fs();

        if !fs.feature(SbFeature::journal_no_flush) {
            return false;
        }

        /*
         * Under j->lock, so in_flight can't move - an entry popped, its slot
         * given to a newer seq - between finding a seq's buf and claiming it.
         * Waiters still attach without it: hence the cmpxchg.
         */
        let g = self.lock.lock();

        /* the first write since the filesystem was clean flushes regardless: */
        if self.flags.get(Journal::need_flush_write) {
            return false;
        }

        if self.lock.flushed_seq_ondisk.get() >= start {
            return false;
        }

        /*
         * What's at or below seq_ondisk is written - and was noflush, or
         * flushed_seq_ondisk would be past it:
         */
        let in_flight = self.in_flight.locked(&g);
        (start.max(in_flight.front_idx())..end)
            .all(|seq| in_flight.get(seq).expect("noflush of a seq not in flight").try_noflush())
    }

    /*
     * Advance the rewind limit so that discards up to @seq become safe.
     * Must be called before bch2_journal_flush() to persist the new limit.
     */
    pub fn advance_rewind_seq(&self, seq: u64) {
        let mut g = self.lock.lock();
        let rewind_seq = self.lock.rewind_seq.get().max(seq);
        g.published().rewind_seq.set(rewind_seq);
    }

    /// Write a journal entry with nothing in it but metadata, and wait for it
    /// to be flushed - without a write ref: meta()'s, for callers that hold one.
    pub fn __meta(&self) -> Result<(), BchError> {
        let mut slot = core::pin::pin!(None);

        /*
         * An empty entry is the journal's own forward progress: it can't wait
         * behind the allocator's free space watermark, which may never drop.
         */
        let mut res = c::journal_res::default();
        let u64s = (size_of::<Head<c::jset_entry>>() / size_of::<u64>()) as u32;
        self.res_get(&mut res, u64s, c::Watermark::reclaim as u32, None)?;
        FlushWait::new(slot.as_mut(), waiter::Sync::current()).attach(self, res.seq);
        self.res_put(&mut res);

        slot.as_mut().as_pin_mut().expect("just made").completion().wait_sync(Sleep::UNINTERRUPTIBLE);

        self.error()
    }

    pub fn meta(&self) -> Result<(), BchError> {
        let fs = self.fs();

        let Some(_write) = fs.writes.tryget_live() else {
            return fs.throw(Errcode::erofs_no_writes);
        };

        self.__meta()
    }

    /// An entry journalled before the journal runs, into its first entry.
    pub fn early_entry_add(&self, ty: c::JsetEntryType, btree_id: u8, level: u8, data: &[u8])
        -> Result<(), BchError>
    {
        let u64s = size_of::<Head<c::jset_entry>>() / size_of::<u64>() + data.len().div_ceil(size_of::<u64>());
        let mut e = KVVec::from_elem(0u64, u64s)?;
        EntryWriter::new(&mut e[..]).push_bytes(ty, btree_id, level, data)
            .expect("an entry, in room sized for it");

        /* j->lock is a spinlock: the entries grow into a new buffer, swapped in under it */
        loop {
            let mut g = self.lock.lock();
            let v = &mut g.early_journal_entries;
            if let Some(dst) = v.extend_zeroed_within_capacity(e.len()) {
                dst.copy_from_slice(&e);
                return Ok(());
            }
            let want = (v.len() + e.len()) * 2;
            drop(g);

            let mut new = KVVec::with_capacity(want)?;

            let mut g = self.lock.lock();
            let v = &mut g.early_journal_entries;
            if v.len() + e.len() <= new.capacity() {
                for s in [&v[..], &e[..]] {
                    new.extend_zeroed_within_capacity(s.len()).expect("room checked").copy_from_slice(s);
                }
                let old = mem::replace(v, new);
                drop(g);
                drop(old);
                return Ok(());
            }
        }
    }

    /* block/unlock the journal: */

    pub fn unblock(&self) {
        {
            let mut g = self.lock.lock();
            let blocked = self.lock.blocked.get() - 1;
            g.published().blocked.set(blocked);
            if blocked == 0 &&
                g.cur_entry_offset_if_blocked < c::JOURNAL_ENTRY_CLOSED_VAL &&
                self.reservations().cur_entry_offset() == c::JOURNAL_ENTRY_BLOCKED_VAL {
                let offset = g.cur_entry_offset_if_blocked;
                let _ = self.reservations_update(|old| {
                    let mut new = old;
                    new.set_cur_entry_offset(offset);
                    Some(new)
                });

                self.fs().times.get(c::TimeStat::blocked_journal_blocked as usize)
                    .track_event_change(false);
            }
        }

        self.wake_async_wait();
    }

    fn __block(&self, g: &mut JournalGuard<'_>) {
        let blocked = self.lock.blocked.get() + 1;
        g.published().blocked.set(blocked);
        if blocked != 1 {
            return;
        }

        let (Ok(old) | Err(old)) = self.reservations_update(|old| {
            g.cur_entry_offset_if_blocked = old.cur_entry_offset();

            let mut new = old;
            new.set_cur_entry_offset(c::JOURNAL_ENTRY_BLOCKED_VAL);
            (old.cur_entry_offset() < c::JOURNAL_ENTRY_CLOSED_VAL).then_some(new)
        });

        if old.cur_entry_offset() < c::JOURNAL_ENTRY_BLOCKED_VAL {
            self.cur_buf(g).data.header(g).set_u64s(old.cur_entry_offset() as usize);
        }
    }

    pub fn block(&self) -> JournalBlocked<'_> {
        self.__block(&mut self.lock.lock());
        self.quiesce();
        JournalBlocked(self)
    }

    /* None while the buf still has reservations to wait for */
    fn __next_write_buffer_flush_journal_buf(&self, max_seq: u64, blocked: &mut bool)
        -> Option<Option<(u64, &c::journal_buf, NonNull<[u8]>)>>
    {
        let mut g = self.lock.lock();
        let cur_seq = self.fs().journal_cur_seq();
        let max_seq = max_seq.min(cur_seq);

        for (seq, buf) in self.in_flight.locked(&g).iter_indexed() {
            if seq > max_seq {
                break;
            }

            if g.get_of(&buf.bits).need_flush_to_write_buffer() {
                let mut s = c::journal_res_state::from_v(self.lock.hot.reservations.load(Acquire));

                let open = u32::from(seq == cur_seq && s.is_open());

                if open != 0 && !*blocked {
                    self.__block(&mut g);
                    s = c::journal_res_state::from_v(self.lock.hot.reservations.load(Acquire));
                    *blocked = true;
                }

                if self.state_seq_count(s, seq) > open {
                    return None;
                }
                return Some(Some((seq, buf, buf.wb_entries(&mut g))));
            }
        }

        Some(None)
    }

    /// The next buf flushing to the write buffer, and its entries, had for as
    /// long as @buf_lock - this journal's - is held.
    pub fn next_write_buffer_flush_journal_buf<'a, 'b>(&'a self, buf_lock: &'b mut c::BufLockGuard<'a>, max_seq: u64,
                                                       blocked: &mut bool)
        -> Option<(u64, &'a c::journal_buf, &'b mut [u8])>
    {
        assert!(ptr::eq(buf_lock.lock(), &self.buf_lock), "the write buffer's flush with another journal's buf_lock");
        *blocked = false;

        let Ok(ret) = self.async_wait.wait_until(Sleep::UNINTERRUPTIBLE,
            || self.__next_write_buffer_flush_journal_buf(max_seq, blocked));

        if ret.is_none() && *blocked {
            self.unblock();
        }

        /*
         * Closed, or blocked, with its reservations put: the body's buf_lock's.
         * need_flush_to_write_buffer still set, its write's task hasn't taken
         * the buffer, nor prepped it - and can't till buf_lock, ours, is
         * dropped.
         */
        let (seq, buf, entries) = ret?;
        Some((seq, buf, unsafe { &mut *entries.as_ptr() }))
    }

    /* debug: */

    #[cold]
    fn __debug_to_text(&self, out: &mut Printbuf, g: &JournalGuard<'_>) {
        use c::JournalSpaceFrom::*;

        let fs = self.fs();
        let now = jiffies();
        let nr_writes = g.nr_flush_writes + g.nr_noflush_writes;

        out.tabstops_reset();
        out.tabstop_push(28);

        let _f = MemallocFlags::noreclaim();
        let rcu = read_lock();

        let s = self.reservations();

        prt!(out, "flags:\t");
        self.flags.to_text(out);
        out.newline();
        prt!(out, "dirty journal entries:\t{}/{}\n", self.pin_range().used(), self.pin_range().pin_size.get());
        prt!(out, "seq:\t{}\n",                         fs.journal_cur_seq());
        prt!(out, "seq_ondisk:\t{}\n",                  self.lock.seq_ondisk.get());
        prt!(out, "last_seq:\t{}\n",                    self.lock.last_seq.get());
        prt!(out, "last_seq_ondisk:\t{}\n",             self.lock.last_seq_ondisk.get());
        prt!(out, "flushed_seq_ondisk:\t{}\n",          self.lock.flushed_seq_ondisk.get());
        prt!(out, "last_empty_seq:\t{}\n",              g.last_empty_seq);
        prt!(out, "rewind_seq:\t{}\n",                  self.lock.rewind_seq.get());
        prt!(out, "rewind_seq_ondisk:\t{}\n",           self.lock.rewind_seq_ondisk.get());
        prt!(out, "watermark:\t");
        c::Watermark::from_bits(self.lock.hot.watermark.get()).to_text(out);
        out.newline();
        prt!(out, "watermark_open_buckets:\t");
        c::Watermark::from_bits(self.lock.watermark_open_buckets.get().into()).to_text(out);
        out.newline();
        prt!(out, "each entry reserved:\t{}\n",         self.lock.entry_u64s_reserved.get());
        prt!(out, "nr flush writes:\t{}\n",             g.nr_flush_writes);
        prt!(out, "nr noflush writes:\t{}\n",           g.nr_noflush_writes);
        prt!(out, "average write size:\t{}\n",
             HumanReadable(if nr_writes != 0 { g.entry_bytes_written / nr_writes } else { 0 }));
        prt!(out, "free buf:\t{}\n",                    g.free_buf.as_ref().map_or(0, |b| b.size()));
        prt!(out, "nr direct reclaim:\t{}\n",           self.reclaim_lock.nr_direct_reclaim.get());
        prt!(out, "nr background reclaim:\t{}\n",       self.reclaim_lock.nr_background_reclaim.get());
        let next_reclaim = self.next_reclaim.get();
        prt!(out, "reclaim runs in:\t{} ms\n",
             if time_after(next_reclaim, now) { jiffies_to_msecs(next_reclaim.wrapping_sub(jiffies())) } else { 0 });
        prt!(out, "blocked:\t{}\n",                     self.lock.blocked.get());
        prt!(out, "flush_wait:\t{}\n",                  u32::from(self.flush_waiting()));
        prt!(out, "current entry sectors:\t{}\n",       g.cur_entry_sectors);
        prt!(out, "current entry error:\t{}\n",         BchError::from_raw(self.lock.cur_entry_error.get()).msg());
        prt!(out, "current entry:\t");

        match s.cur_entry_offset() {
            c::JOURNAL_ENTRY_ERROR_VAL   => prt!(out, "error\n"),
            c::JOURNAL_ENTRY_CLOSED_VAL  => prt!(out, "closed\n"),
            c::JOURNAL_ENTRY_BLOCKED_VAL => prt!(out, "blocked\n"),
            offset => prt!(out, "{offset}/{}\n", self.lock.hot.cur_entry_u64s.get()),
        }

        prt!(out, "unwritten entries:\n");
        self.bufs_to_text(out, g);

        prt!(out, "space:\n");
        {
            let mut indent = out.indent(2);
            let out = &mut *indent;
            for (name, from) in [("discarded",    journal_space_discarded),
                                 ("clean ondisk", journal_space_clean_ondisk),
                                 ("clean",        journal_space_clean),
                                 ("total",        journal_space_total)] {
                let space = &g.space[from as usize];
                prt!(out, "{name}\t{}:{}\n", space.next_entry, space.total);
            }
        }

        for ca in fs.rw_devs_for_rcu(DataType::journal, &rcu) {
            let ja = &ca.journal;
            let ring = ca.journal_ring(g);
            if ring.nr() == 0 {
                continue;
            }

            prt!(out, "dev {}:\n",                      ca.dev_idx);
            prt!(out, "durability {}:\n",               ca.mi.get().durability);
            let mut indent = out.indent(2);
            let out = &mut *indent;
            prt!(out, "nr\t{}\n",                       ring.nr());
            prt!(out, "bucket size\t{}\n",              ca.mi.get().bucket_size);
            prt!(out, "available\t{}:{}\n",             ring.available(journal_space_discarded), ja.sectors_free.get());
            ring.to_text(out);
        }

        prt!(out, "replicas {}\n", fs.journal_replicas());
    }

    #[cold]
    pub fn debug_to_text(&self, out: &mut Printbuf) {
        self.__debug_to_text(out, &self.lock.lock());

        prt!(out, "Oldest journal pins:\n");
        self.pins_to_text(out, 2);

        if self.flags.get(Journal::low_on_wb) {
            self.fs().btree_write_buffer_to_text(out);
        }
    }
}

/*
 * The journal auto-commit timer: re-armed for journal_flush_delay whenever a
 * flush write is submitted (in do_writes_locked()), so it only
 * actually fires once that long has passed with no commit - at which point we
 * close the open entry to get it written out.
 */
pub(crate) fn journal_write_work(j: &c::journal) {
    j.flush_async();
}

impl NotRw<'_> {
    pub(crate) fn journal_add_rewind_range(&mut self, from: u64, to: u64) -> Result<(), BchError> {
        let fs = self.fs;
        self.get_mut_of(&fs.journal.rewind_ranges).push(c::journal_rewind_range { from, to })?;

        fs.journal.early_entry_add(c::JsetEntryType::rewind, 0, 0,
                                   [from.to_le(), to.to_le()].as_bytes())
    }
}

/*
 * Slowpath: wake a journal flush waitlist after the journal errored.
 *
 * Sentinel-safe: buf->wait multiplexes journal sentinels
 * (NOT_IN_FLIGHT/NOFLUSH/FLUSH_NO_WAIT) with waiter chains, so grab into a
 * local and wake only real waiters. xchg to NULL races cleanly with the
 * journal_write_done{,_flush}() grabbers - they just get NULL, so no sentinel
 * is ever walked.
 */
#[cold]
fn flush_seq_err_wake(wait: &AtomicPtr<FlushEntry>) {
    if let Some(waiters) = Waiters::take(wait, ptr::null_mut()) {
        waiters.wake();
    }
}
