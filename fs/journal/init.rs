// SPDX-License-Identifier: GPL-2.0

//! The journal's startup and shutdown, and what it allocates going rw.

use core::sync::atomic::Ordering::Relaxed;

use crate::alloc::replicas::DevlistEntry;
use crate::btree::bkey::prelude::*;
use crate::c;
use crate::c::journal_flags::*;
use crate::data::ptrs::Ptrs;
use crate::errcode::{bch_errcode, BchError};
use crate::format::DataType;
use crate::fs::{Dev, StateLockWrite};
use crate::init::error::id;
use crate::types::Fifo;
use crate::util::alloc::darray_take;
use crate::util::bio::BioSet;
use crate::util::closure::closure_wait_event;
use crate::util::kernel::jiffies;
use crate::{bch_err, fsck_err_on, warn_on};

/* journal.h's JOURNAL_BUF_NOT_IN_FLIGHT: a buf's wait list, while it isn't */
const NOT_IN_FLIGHT: *mut c::llist_node = 1 as *mut c::llist_node;

impl c::journal {
    /* startup/shutdown: */

    fn writing_to_device(&self, dev_idx: u32) -> bool {
        let fs = self.fs();
        let _j = self.lock.lock();

        self.in_flight.iter().any(|buf| {
            buf.key().as_sc().arm::<Ptrs>().is_some_and(|k| k.has_device(fs, dev_idx).is_some())
        })
    }

    pub fn dev_stop(&self, ca: &Dev) {
        closure_wait_event(&self.async_wait, || !self.writing_to_device(ca.dev_idx.into()));
        unsafe { c::cancel_work_sync((&raw const ca.journal.discard).cast_mut()) };
    }

    pub fn stop(&self) {
        if !self.flags.get(JOURNAL_running) {
            return;
        }

        unsafe { c::bch2_journal_reclaim_stop(self.as_ptr()) };

        /* userspace's current is C's __thread variable, which Rust can't name: unchecked there */
        #[cfg(CONFIG_BCACHEFS_DEBUG)]
        {
            #[cfg(kernel)]
            self.stop_thread.store(unsafe { c::get_current() }, Relaxed);
            #[cfg(not(kernel))]
            self.stop_thread.store(core::ptr::null_mut(), Relaxed);
        }
        self.flush_pins(u64::MAX);

        /*
         * Always write a new journal entry, to make sure the clock hands are up
         * to date (and match the superblock)
         */
        unsafe { c::__bch2_journal_meta(self.as_ptr()) };

        unsafe { c::bch2_journal_shutdown_quiesce(self.as_ptr()) };
        self.write_work.cancel_sync();

        #[cfg(CONFIG_BCACHEFS_DEBUG)]
        self.stop_thread.store((-(c::EROFS as isize)) as *mut c::task_struct, Relaxed);

        let fs = self.fs();
        let replay_done = self.error().is_ok() && self.flags.get(JOURNAL_replay_done);
        let (last_empty_seq, dirty_entry_bytes) = {
            let g = self.lock.lock();
            (g.last_empty_seq, g.dirty_entry_bytes)
        };

        let cur_seq = fs.journal_cur_seq();
        if replay_done && last_empty_seq != cur_seq {
            bch_err!(fs, "journal shutdown error: cur seq {cur_seq} but last empty seq {last_empty_seq}");
            warn_on!(true);
        }

        /*
         * All pins have been flushed and the journal quiesced, so every dirty
         * entry has been reclaimed - dirty_entry_bytes must have drained to 0.
         * A nonzero remainder means the accounting (journal.c entry-close +/
         * bch2_journal_update_last_seq_ondisk -) has leaked, which silently
         * shrinks the in-memory journal-space budget (see mem_limit in
         * bch2_journal_space_available()).
         */
        if replay_done && dirty_entry_bytes != 0 {
            bch_err!(fs, "journal shutdown error: {dirty_entry_bytes} dirty entry bytes remaining after flushing all pins");
            warn_on!(true);
        }

        if self.error().is_ok() {
            self.flags.set(JOURNAL_running, false);
        }
    }

    pub fn start(&self, info: c::journal_start_info) -> Result<(), BchError> {
        let fs = self.fs();

        /* Don't reuse sequence numbers that are blacklisted */
        let cur_seq = info.cur_seq.max(fs.journal_seq_blacklist.last_blacklisted() + 1);

        if cur_seq >= c::JOURNAL_SEQ_MAX {
            bch_err!(fs, "cannot start: journal seq overflow");
            return fs.throw(bch_errcode::BCH_ERR_EINVAL_journal_seq_overflow);
        }

        /* Clean filesystem? */
        let last_seq = if info.last_seq != 0 { info.last_seq } else { cur_seq };

        crate::journal::pins::start(self, last_seq, cur_seq)?;

        self.replay_journal_seq.set(last_seq);
        self.flushed_seq_ondisk.set(cur_seq - 1);
        if self.rewind_seq.get() == 0 {
            self.rewind_seq.set(last_seq);
            self.rewind_seq_ondisk.set(last_seq);
        }
        {
            let mut g = self.lock.lock();
            g.seq_write_started = cur_seq - 1;
            let p = g.published();
            p.replay_journal_seq_end.set(cur_seq);
            p.last_seq_ondisk.set(last_seq);
            p.seq_ondisk.set(cur_seq - 1);
            p.last_seq.set(last_seq);
        }
        self.lock.seq.store((cur_seq - 1) as i64, Relaxed);
        self.lock.hot.res_seq.store((cur_seq - 1) as i64, Relaxed);

        /*
         * Align the in_flight FIFO with seq numbering: front == seq_ondisk + 1
         * == cur_seq initially (empty fifo), so fifo_entry(in_flight, seq)
         * indexes by seq once entries start pushing.
         */
        let in_flight = unsafe { self.in_flight_before_rw() };
        in_flight.front = cur_seq;
        in_flight.back = cur_seq;

        let mut had_entries = false;
        let jk = fs.journal_keys_get();
        for i in jk.iter().flat_map(|jk| jk.value().entries().iter()) {
            if i.ignored() {
                continue;
            }

            let seq = i.seq();
            assert!(seq < cur_seq, "journal entry {seq} at or past the journal's start, {cur_seq}");

            if seq < last_seq {
                continue;
            }

            if i.jset_unvalidated().empty() {
                self.lock.lock().last_empty_seq = seq;
            }

            let seq_devs = if !info.clean { c::bch2_journal_replay_devs(fs, i) } else { Default::default() };

            if seq_devs.nr != 0 {
                let r = DevlistEntry::new(DataType::journal, seq_devs);

                fsck_err_on!(fs, !self.flags.get(JOURNAL_degraded) && !fs.replicas_marked(&r),
                             id::journal_entry_replicas_not_marked,
                             "superblock not marked as containing replicas for journal entry {}\n{}",
                             seq, *r)?;

                if fs.replicas_entry_get(&r).is_ok() {
                    self.pins().seq_set_devs(seq, &seq_devs);
                }
            }

            had_entries = true;
        }

        {
            let mut g = self.lock.lock();
            if !had_entries {
                g.last_empty_seq = cur_seq - 1; /* to match j->seq */
            }

            g.last_flush_write = jiffies();
            let r = &self.lock.hot.reservations;
            let mut s = c::journal_res_state_bits::from_bits(r.load(Relaxed));
            s.set_idx((fs.journal_cur_seq() & c::JOURNAL_STATE_BUF_MASK as u64) as u8);
            r.store(s.into_bits(), Relaxed);
        }

        fs.replicas_gc_reffed()
    }

    pub fn set_replay_done(&self) {
        /*
         * journal_space_available must happen before setting JOURNAL_running
         * JOURNAL_running must happen before JOURNAL_replay_done
         */
        let _j = self.lock.lock();
        unsafe { c::bch2_journal_space_available(self.as_ptr()) };

        self.flags.set(JOURNAL_need_flush_write, true);
        self.flags.set(JOURNAL_running, true);
        self.flags.set(JOURNAL_replay_done, true);
    }

    /// # Safety
    /// Before we're rw, and before anything's pushed: no one else's.
    unsafe fn in_flight_before_rw(&self) -> &mut Fifo<c::journal_buf, u64> {
        assert!(!self.fs().flags.get(c::bch_fs_flags::BCH_FS_rw), "journal in_flight fifo set up while rw");
        unsafe { &mut *(&raw const self.in_flight).cast_mut() }
    }

    /* init/exit: */

    pub fn init_rw(&self) -> Result<(), BchError> {
        let fs = self.fs();
        let enomem = || fs.err(bch_errcode::BCH_ERR_ENOMEM_journal_buf);
        let size = c::JOURNAL_ENTRY_SIZE_MIN;

        self.buf_size_want.set(size);
        let buf = c::JsetBuf::new(size).map_err(|_| enomem())?;
        self.lock.lock().free_buf = buf;

        /*
         * in_flight holds one entry per seq in (seq_ondisk, cur_seq]. Size is
         * an upper bound on in-flight journal writes; starts at 256, which is
         * well above the 16-slot cap of the prior static-array layout. If
         * workloads saturate this, journal_entry_open() returns ENOMEM and
         * the existing retry path throttles via completion backpressure.
         *
         * Not reset: start() already set front/back to the correct
         * seq-aligned value. Every slot is a buf not in flight - the one at
         * back too, which the flushers' walk stops at.
         */
        let in_flight = unsafe { self.in_flight_before_rw() };
        in_flight.reserve(256).map_err(|_| enomem())?;
        in_flight.init_spare(|buf| unsafe {
            (&raw mut (*buf.as_mut_ptr()).wait.list.first).write(NOT_IN_FLIGHT)
        });
        Ok(())
    }

    /// # Safety
    /// The filesystem's going away: nothing else touches the journal.
    pub unsafe fn exit(j: *mut c::journal) {
        let j = unsafe { &mut *j };

        drop(unsafe { darray_take(&mut j.early_journal_entries) });
        drop(unsafe { darray_take(&mut j.rewind_ranges) });

        unsafe {
            c::cancel_work_sync(&raw mut j.pin_resize_work);
            c::cancel_work_sync(&raw mut j.flush_degraded_work);
        }
    }
}

impl Dev {
    pub fn journal_init(&self, state: &StateLockWrite<'_>) -> Result<(), BchError> {
        /*
         * Pool size is just the mempool reserve for forward progress under
         * memory pressure; JOURNAL_STATE_BUF_NR matches the reservation
         * fastpath ring (the effective parallel-write ceiling for hot-path
         * allocations).
         */
        self.journal.bio_set.init(state, || BioSet::new(c::JOURNAL_STATE_BUF_NR, true))
            .map_err(|_| self.fs().err(bch_errcode::BCH_ERR_ENOMEM_dev_journal_init))
    }
}
