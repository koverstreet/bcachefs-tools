// SPDX-License-Identifier: GPL-2.0

//! Journal reclaim: space accounting, discards, and flushing pins - by the
//! reclaim thread, or directly.

use core::ffi::c_int;
use core::sync::atomic::Ordering::{Acquire, Relaxed};

use crate::alloc::discard::discard_opt_enabled;
use crate::btree::cache::BTREE_WRITE_IO_LIMIT;
use crate::c;
use crate::c::JournalBucketStage::{JOURNAL_BUCKET_cur, JOURNAL_BUCKET_discard};
use crate::c::JournalFlag as Journal;
use crate::c::JournalPinType::{JOURNAL_PIN_TYPE_btree0, JOURNAL_PIN_TYPE_key_cache, JOURNAL_PIN_TYPE_NR};
use crate::c::JournalSpaceFrom::{self as SpaceFrom, *};
use crate::c::JournalGuard;
use crate::data_structures::array_vec::ArrayVec;
use crate::debug::trace::events;
use crate::errcode::{Errcode, BchError};
use crate::format::{DataType, SbFeature};
use crate::fs::{Dev, Fs};
use crate::journal::pins::PinsRead;
use crate::locking::{ExclusiveGuard, MemallocFlags};
use crate::util::backtrace::prt_task_backtrace;
use crate::util::closure::{closure_wait_event, closure_wake_up};
use crate::os::kernel::{bdev_max_discard_sectors, blkdev_issue_discard, cond_resched, jiffies, jiffies_to_msecs,
                          msecs_to_jiffies, time_after, time_in_range, totalram_bytes, HZ};
use crate::util::kthread::{Kthread, KthreadSelf};
use crate::util::place::Contained;
use crate::util::rcu::read_lock;
use crate::util::sleep::{sleep_until, Sleep, SleepError};
use crate::util::time_stats::TimeStats;
use crate::util::{Nsecs, Printbuf};
use crate::{bch_err, bch_err_msg, prt};

type ReclaimGuard<'a> = ExclusiveGuard<'a, c::mutex, (), c::ReclaimPublished>;

fn round_down(x: u32, m: u32) -> u32 {
    x / m * m
}

impl c::journal {
    /// The lowest watermark it takes reservations at, read without the
    /// journal's lock, which it's written under.
    pub fn watermark(&self) -> c::Watermark {
        c::Watermark::from_bits(self.lock.hot.watermark.get())
    }

    /// The open bucket pool can't serve below @watermark - 0 for all of them:
    /// the journal throttles work that would need what it can't have. The
    /// unchanged case is a compare: the allocator's fast paths report on
    /// every bucket.
    pub fn set_open_buckets_watermark(&self, watermark: u32) {
        let p = &self.lock.watermark_open_buckets;
        if u32::from(p.get()) != watermark {
            let mut g = self.lock.lock();
            /* Recheck: two reporters can see the same crossing. */
            if u32::from(p.get()) != watermark {
                g.published().watermark_open_buckets.set(watermark as u8);
                self.set_watermark(&mut g);
            }
        }
    }

    pub fn set_watermark(&self, g: &mut JournalGuard<'_>) {
        use c::TimeStat as Time;
        let fs = self.fs();
        let clean = u64::from(g.space[journal_space_clean as usize].total);
        let total = u64::from(g.space[journal_space_total as usize].total);

        let med_on_space = clean * 4 <= total * 3;

        let low_on_space = clean * 4 <= total;
        let low_on_pin = self.pin_range().used() > self.pin_limit() / 4 * 3;
        let low_on_wb = fs.btree_write_buffer_must_wait();

        let open_buckets = self.lock.watermark_open_buckets.get();
        let watermark = if low_on_space || low_on_pin || low_on_wb {
            c::Watermark::reclaim
        } else {
            c::Watermark::stripe
        } as u32;

        let watermark = watermark.max(open_buckets.into());

        let times = |t: c::TimeStat| &fs.times[t as usize];
        let mut changed = false;
        changed |= times(Time::blocked_journal_low_on_space).track_event_change(low_on_space);
        changed |= times(Time::blocked_journal_low_on_pin).track_event_change(low_on_pin);
        changed |= times(Time::blocked_journal_low_on_open_buckets).track_event_change(open_buckets > 0);
        changed |= times(Time::blocked_write_buffer_full).track_event_change(low_on_wb);

        if changed {
            fs.event_inc_trace(events::journal_full, |out| {
                let _f = MemallocFlags::noreclaim();
                prt!(out, "low_on_space {}\n", low_on_space as u32);
                prt!(out, "low_on_pin {}\n", low_on_pin as u32);
                prt!(out, "watermark_open_buckets {open_buckets}\n");
                prt!(out, "low_on_wb {}\n", low_on_wb as u32);
                if low_on_wb {
                    fs.btree_write_buffer_to_text(out);
                }
            });
        }

        self.flags.set(Journal::med_on_space, med_on_space);

        self.flags.set(Journal::low_on_space, low_on_space);
        self.flags.set(Journal::low_on_pin, low_on_pin);
        self.flags.set(Journal::low_on_wb, low_on_wb);

        let old = self.lock.hot.watermark.get();
        g.published().hot.watermark.set(watermark);
        if old > watermark {
            self.wake_async_wait();
        }
        if med_on_space {
            self.reclaim_kick();
        }
    }

    pub fn wake_async_wait(&self) {
        closure_wake_up(&self.async_wait);
    }

    fn dev_space_available(&self, g: &JournalGuard<'_>, ca: &Dev, from: SpaceFrom) -> c::journal_space {
        let fs = self.fs();
        let ja = &ca.journal;
        let bucket_size_aligned = round_down(ca.mi.get().bucket_size.into(), fs.block_sectors());

        /*
         * Both .total values are capped by the in-memory dirty budget: we
         * can't keep more than RAM/4 of journal dirty in memory, so the space
         * the watermark sees is bounded by RAM, not by raw device capacity.
         * (A global budget, applied per-device; the nr_devs_want-th largest
         * pick in space_available_from() keeps the aggregate bounded.)
         *
         * The cap is asymmetric, and that asymmetry is the point: the watermark
         * ratio (clean * 4 <= total, see set_watermark()) has to
         * measure how full the dirty budget is, not how big the disk is. So:
         *
         *  - journal_space_total is capped at a flat RAM/4 - dirty is NOT
         *    subtracted, so it's a constant ceiling.
         *  - journal_space_clean/_clean_ondisk are capped at RAM/4 - dirty,
         *    so they shrink as dirty grows, and the ratio falls as the budget
         *    fills. That's the intended soft throttle.
         *
         * Both halves matter. Leaving journal_space_total at true device
         * capacity (as it once was) breaks it the other way: any fs whose
         * journal is larger than RAM has clean permanently small vs total, so
         * low_on_space is stuck on even when the journal is nearly empty.
         * Conversely, subtracting dirty from journal_space_total too would let
         * total shrink toward 0 as dirty grows and strangle the journal. A flat
         * RAM/4 ceiling for total is what keeps both failure modes away.
         *
         * Only .total is clamped, not next_entry: .total feeds the watermark
         * (a soft throttle - low-priority writers wait, reclaim-priority writes
         * still proceed), whereas next_entry feeds cur_entry_sectors, the hard
         * reservation limit. Clamping next_entry would let the budget drive
         * cur_entry_sectors to 0, blocking even the journal writes that advance
         * last_seq and let dirty drain - a self-deadlock. The RAM budget must
         * stay a soft limit.
         */
        let mem_limit = totalram_bytes() / 4;
        let capped = |sectors: u64, mem_limit: usize| sectors.min((mem_limit >> 9) as u64) as u32;

        if from == journal_space_total {
            return c::journal_space {
                next_entry: bucket_size_aligned,
                total:      capped(u64::from(bucket_size_aligned) * u64::from(ja.nr()), mem_limit),
            };
        }

        let mut buckets = ca.journal_ring(g).available(from);
        let mut sectors = round_down(ja.sectors_free.get(), fs.block_sectors());

        /*
         * We that we don't allocate the space for a journal entry
         * until we write it out - thus, account for it here:
         */
        for buf in g.in_flight.iter() {
            let unwritten = buf.sectors;
            if unwritten == 0 {
                continue;
            }

            /* entry won't fit on this device, skip: */
            if unwritten > bucket_size_aligned {
                continue;
            }

            if unwritten >= sectors {
                if buckets == 0 {
                    sectors = 0;
                    break;
                }

                buckets -= 1;
                sectors = bucket_size_aligned;
            }

            sectors -= unwritten;
        }

        if sectors < ca.mi.get().bucket_size.into() && buckets != 0 {
            buckets -= 1;
            sectors = bucket_size_aligned;
        }

        let mem_limit = mem_limit.saturating_sub(g.dirty_entry_bytes);

        c::journal_space {
            next_entry: sectors,
            total:      capped(u64::from(sectors) + u64::from(buckets) * u64::from(bucket_size_aligned), mem_limit),
        }
    }

    fn space_available_from(&self, g: &JournalGuard<'_>, nr_devs_want: usize, from: SpaceFrom) -> c::journal_space {
        let fs = self.fs();
        /* one past: an insert comes before the truncate */
        let mut largest = ArrayVec::<c::journal_space, { c::BCH_REPLICAS_MAX as usize + 1 }>::new();
        let mut nr_devs = 0;
        let mut min_bucket_size = u32::MAX;

        assert!(nr_devs_want <= c::BCH_REPLICAS_MAX as usize, "journal space for {nr_devs_want} devices");

        let rcu = read_lock();
        for ca in fs.rw_devs_for_rcu(DataType::journal, &rcu) {
            if ca.journal.nr() == 0 {
                continue;
            }

            min_bucket_size = min_bucket_size.min(ca.mi.get().bucket_size.into());

            let space = self.dev_space_available(g, ca, from);
            if space.next_entry == 0 {
                continue;
            }

            /*
             * Only the @nr_devs_want largest matter: top-k insertion
             * sort, largest to smallest, entries past nr_devs_want fall
             * off. nr_devs still counts every qualifying device.
             */
            let pos = largest.iter().position(|s| space.total > s.total).unwrap_or(largest.len());
            if pos < nr_devs_want {
                largest.insert(pos, space);
                largest.truncate(nr_devs_want);
            }
            nr_devs += 1;
        }

        if nr_devs < nr_devs_want {
            return c::journal_space::default();
        }

        /*
         * It's possible for bucket size to be misaligned w.r.t. the filesystem
         * block size:
         */
        let min_bucket_size = round_down(min_bucket_size, fs.block_sectors());

        /*
         * We sorted largest to smallest, and we want the smallest out of the
         * @nr_devs_want largest devices:
         */
        let mut space = largest[nr_devs_want - 1];
        space.next_entry = space.next_entry.min(min_bucket_size);
        space
    }

    pub fn space_available(&self, g: &mut JournalGuard<'_>) {
        let fs = self.fs();
        let mut nr_online = 0;
        let mut can_discard = false;

        {
            let rcu = read_lock();
            for ca in fs.rw_devs_for_rcu(DataType::journal, &rcu) {
                let ja = &ca.journal;

                if ja.nr() == 0 {
                    continue;
                }

                ca.journal_ring_mut(g).last_seq(self.lock.last_seq.get(), self.lock.last_seq_ondisk.get());

                nr_online += 1;

                let should_discard = self.should_discard_bucket(ca.journal_ring(g));
                can_discard |= should_discard;

                if should_discard &&
                    fs.flags.get(c::FsFlag::rw_init_done) &&
                    self.flags.get(Journal::running) &&
                    self.error().is_ok() {
                    ja.discard.queue(&self.discard_wq);
                }
            }
        }

        g.published().can_discard.set(can_discard);

        if nr_online == 0 {
            if !fs.feature(SbFeature::small_image) {
                let _f = MemallocFlags::noreclaim();
                let mut buf = Printbuf::new();
                prt!(&mut buf, "no writeable journal devices available\n");

                let rcu = read_lock();
                for ca in fs.rw_devs_for_rcu(DataType::journal, &rcu) {
                    prt!(&mut buf, " {ca}");
                }

                bch_err!(fs, "{buf}");
            }

            g.cur_entry_sectors = 0;
            g.published().cur_entry_error.set(-fs.err(Errcode::insufficient_journal_devices).raw());
            return;
        }

        let nr_devs_want = nr_online.min(fs.journal_replicas() as usize);

        for from in [journal_space_discarded, journal_space_clean_ondisk, journal_space_clean, journal_space_total] {
            let space = self.space_available_from(g, nr_devs_want, from);
            g.space[from as usize] = space;
        }

        let space = |from: SpaceFrom| g.space[from as usize];
        let clean_ondisk = space(journal_space_clean_ondisk).total;
        let clean        = space(journal_space_clean).total;
        let total        = space(journal_space_total).total;

        let may_skip_flush = space(journal_space_clean_ondisk).next_entry < clean_ondisk &&
            clean.wrapping_sub(clean_ondisk) <= total / 8 &&
            u64::from(clean_ondisk) * 2 > u64::from(clean);
        self.flags.set(Journal::may_skip_flush, may_skip_flush);

        self.set_watermark(g);

        g.cur_entry_sectors = g.space[journal_space_discarded as usize].next_entry;
        let cur_entry_error = if g.cur_entry_sectors != 0 {
            0
        } else {
            -fs.err(Errcode::journal_full).raw()
        };
        g.published().cur_entry_error.set(cur_entry_error);

        if cur_entry_error == 0 {
            self.wake_async_wait();
        }
    }

    /* Discards - last part of journal reclaim: */

    /* And the bucket to discard, read with the lock held: resizing moves them */
    fn bucket_to_discard(&self, ca: &Dev) -> Option<u64> {
        let g = self.lock.lock();
        let ring = ca.journal_ring(&g);
        self.should_discard_bucket(ring).then(|| ring.bucket(JOURNAL_BUCKET_discard))
    }

    fn dev_do_discards(&self, ca: &Dev) {
        let fs = self.fs();

        let Some(ca) = fs.dev_get_ioref::<c::DevWriteRefKind>(ca.dev_idx.into()) else {
            return;
        };
        let _d = ca.journal.discard_lock.lock();

        while let Some(bucket) = self.bucket_to_discard(&ca) {
            let bdev = ca.disk_sb.bdev();
            if fs.opts().nochanges.get() == 0 &&
                discard_opt_enabled(fs, &ca) &&
                bdev_max_discard_sectors(bdev) != 0 {
                let _ = blkdev_issue_discard(bdev, ca.bucket_to_sector(bucket),
                                             ca.mi.get().bucket_size.into(), c::GFP_NOIO);
            }

            let mut g = self.lock.lock();
            ca.journal_ring_mut(&mut g).discarded(bucket);
            self.space_available(&mut g);
        }
    }

    pub fn do_discards(&self) {
        for ca in self.fs().members() {
            self.dev_do_discards(&ca);
        }
    }

    /*
     * Journal entry pinning - machinery for holding a reference on a given journal
     * entry, holding it open to ensure it gets replayed during recovery:
     */

    pub fn update_last_seq(&self, pins: &PinsRead<'_>, g: &mut JournalGuard<'_>) {
        /*
         * Unpin journal entries whose reference counts reached zero, meaning
         * all btree nodes got written out
         */
        let old = self.lock.last_seq.get();
        let mut last_seq = old;
        while last_seq < pins.back() &&
            last_seq <= self.lock.seq_ondisk.get() &&
            pins.seq_unpinned(last_seq) {
            last_seq += 1;
        }

        if old != last_seq {
            g.published().last_seq.set(last_seq);
            self.space_available(g);
            closure_wake_up(&self.reclaim_flush_wait);
        }
    }

    pub fn replay_pins_put(&self, seq: u64) {
        let start = self.replay_journal_seq.get();
        assert!(seq >= start, "journal replay putting pins up to {seq}, behind {start}");

        let seq = seq.min(self.lock.replay_journal_seq_end.get());

        let pins = self.pins();

        while self.replay_journal_seq.get() < seq {
            let s = self.replay_journal_seq.get();
            self.replay_journal_seq.set(s + 1);

            if pins.seq_replayed(s) {
                self.update_last_seq(&pins, &mut self.lock.lock());
            }
        }
    }

    /* ensure journal pin callback is no longer running */
    pub fn pin_flush(&self, pin: &c::journal_entry_pin) {
        assert!(pin.seq.get() == 0, "flushing journal pin at seq {}, still pinned", pin.seq.get());

        let Ok(()) = self.pin_flush_wait.wait_until(Sleep::UNINTERRUPTIBLE,
            || (self.flush_in_progress.load(Acquire) != pin.as_ptr()).then_some(()));
    }

    /*
     * Journal reclaim: flush references to open journal entries to reclaim space in
     * the journal
     *
     * May be done by the journal code in the background as needed to free up space
     * for more journal entries, or as part of doing a clean shutdown, or to migrate
     * data off of a specific device:
     */

    /* returns true if we did work */
    fn flush_pins_locked(&self, reclaim: &mut ReclaimGuard<'_>, seq_to_flush: u64, allowed_below_seq: u32,
                         mut min_any: usize, mut min_key_cache: usize) -> usize {
        use c::TimeStat as Time;
        let fs = self.fs();
        let mut nr_flushed = 0;

        loop {
            let any = if min_any != 0 { !0 } else { 0 };
            let key_cache = if min_key_cache != 0 { 1 << JOURNAL_PIN_TYPE_key_cache as u32 } else { 0 };
            let allowed_above = any | key_cache;

            cond_resched();

            reclaim.published().last_flushed.set(jiffies());

            let Some(mut flushing) =
                self.pins().next_to_flush(seq_to_flush, allowed_below_seq | allowed_above, allowed_above) else {
                break;
            };
            let ty = flushing.ty;

            if min_key_cache != 0 && ty == JOURNAL_PIN_TYPE_key_cache {
                min_key_cache -= 1;
            }

            min_any = min_any.saturating_sub(1);

            let flush_time = if (ty as u32) <= JOURNAL_PIN_TYPE_btree0 as u32 {
                Time::journal_pin_flush_btree
            } else if ty == JOURNAL_PIN_TYPE_key_cache {
                Time::journal_pin_flush_key_cache
            } else {
                Time::journal_pin_flush_other
            };
            let ok = fs.times[flush_time as usize].time(|| flushing.run());

            drop(flushing);

            if !ok {
                break;
            }

            nr_flushed += 1;
        }

        nr_flushed
    }

    fn seq_to_flush(&self) -> u64 {
        let fs = self.fs();
        let g = self.lock.lock();
        let rcu = read_lock();

        let seq_to_flush = fs.rw_devs_rcu(&rcu)
            .filter(|ca| ca.journal.nr() != 0)
            /* Try to keep the journal at most half full: */
            .map(|ca| ca.journal_ring(&g).seq_ahead(JOURNAL_BUCKET_cur, ca.journal.nr() / 2))
            .max()
            .unwrap_or(0);

        /* Also flush if the pin fifo is more than half full */
        seq_to_flush.max(fs.journal_cur_seq().saturating_sub(self.pin_limit() / 2))
    }

    /*
     * free up journal buckets
     * @direct:	direct or background reclaim?
     * @kicked:	requested to run since we last ran?
     *
     * Background journal reclaim writes out btree nodes. It should be run
     * early enough so that we never completely run out of journal buckets.
     *
     * High watermarks for triggering background reclaim:
     * - FIFO has fewer than 512 entries left
     * - fewer than 25% journal buckets free
     *
     * Background reclaim runs until low watermarks are reached:
     * - FIFO has more than 1024 entries left
     * - more than 50% journal buckets free
     *
     * As long as a reclaim can complete in the time it takes to fill up
     * 512 journal entries or 25% of all journal buckets, then
     * journal_next_bucket() should not stall.
     */
    fn reclaim_pass(&self, reclaim: &mut ReclaimGuard<'_>, direct: bool, kicked: bool) -> Result<(), BchError> {
        let fs = self.fs();
        let bc = &fs.btree.cache;
        let kc = &fs.btree.key_cache;

        /*
         * We can't invoke memory reclaim while holding the reclaim_lock -
         * journal reclaim is required to make progress for memory reclaim
         * (cleaning the caches), so we can't get stuck in memory reclaim while
         * we're holding the reclaim lock:
         */
        let _noio = MemallocFlags::noio();

        while !Kthread::should_stop() {
            self.error()?;

            closure_wait_event(&bc.nr_in_flight_wait,
                || bc.nr_in_flight_inner.load(Relaxed) < BTREE_WRITE_IO_LIMIT as isize);

            let seq_to_flush = self.seq_to_flush();

            let btree_cache_live = bc.lock.nr_live();
            let btree_cache_dirty = bc.lock.nr_dirty();
            let delay = msecs_to_jiffies(fs.opts().journal_reclaim_delay.get().into());

            /*
             * If it's been longer than j->reclaim_delay_ms since we last flushed,
             * make sure to flush at least one journal pin:
             */
            let min_nr = (time_after(jiffies(), self.reclaim_lock.last_flushed.get().wrapping_add(delay)) ||
                          self.flags.get(Journal::med_on_space) ||
                          btree_cache_dirty * 2 > btree_cache_live) as usize;

            let min_key_cache = kc.nr_keys_need_flush().min(128);

            fs.event_inc_trace(events::journal_reclaim_start, |out| {
                prt!(out, "direct {} kicked {}\n", direct as u32, kicked as u32);
                prt!(out, "btree cache {btree_cache_dirty}/{btree_cache_live} min {min_nr}\n");
                prt!(out, "key cache {}/{} min {min_key_cache}\n",
                     kc.nr_dirty.load(Relaxed), kc.nr_keys.load(Relaxed));
            });

            let nr_flushed = self.flush_pins_locked(reclaim, seq_to_flush, !0, min_nr, min_key_cache);

            let p = reclaim.published();
            let nr = if direct { &p.nr_direct_reclaim } else { &p.nr_background_reclaim };
            nr.set(nr.get() + nr_flushed as u64);

            fs.event_inc_trace(events::journal_reclaim_finish, |out| prt!(out, "flushed {nr_flushed}"));

            if nr_flushed != 0 {
                self.reclaim_wait.wake();
            }

            if !((min_nr != 0 || min_key_cache != 0) && nr_flushed != 0 && !direct) {
                break;
            }
        }

        Ok(())
    }

    pub fn reclaim(&self, reclaim: &mut ReclaimGuard<'_>) -> Result<(), BchError> {
        self.reclaim_pass(reclaim, true, true)
    }

    pub fn reclaim_stop(&self) {
        self.reclaim_thread.stop();
    }

    pub fn reclaim_start(&self) -> Result<(), BchError> {
        let fs = self.fs();

        if self.reclaim_thread.started() {
            self.reclaim_thread.wake();
            return Ok(());
        }

        let fs: &'static Fs = unsafe { &*fs.as_ptr() };
        let ret = self.reclaim_thread.start(format_args!("bch-reclaim/{}", fs.name().to_str().unwrap_or("?")),
                                            move |me| reclaim_thread(&fs.journal, me));
        bch_err_msg!(fs, ret, "creating journal reclaim thread")
    }

    pub fn reclaim_kick(&self) {
        self.reclaim_thread.kick();
    }

    /// For a commit the key cache has too many dirty keys for: until reclaim
    /// has flushed enough, or the journal's halted. @unlock_long runs if that
    /// takes more than a second.
    pub fn key_cache_reclaim_wait(&self, unlock_long: impl FnOnce()) -> Result<(), BchError> {
        let mut done = || {
            if let Err(e) = self.error() {
                return Some(Err(e));
            }
            if self.fs().btree.key_cache.wait_done() {
                return Some(Ok(()));
            }
            self.reclaim_kick();
            None
        };

        match self.reclaim_wait.wait_until(Sleep::INTERRUPTIBLE.freezable().timeout(HZ), &mut done) {
            Ok(r) => r,
            Err(SleepError::Timeout) => {
                unlock_long();
                self.reclaim_wait.wait_until(Sleep::INTERRUPTIBLE.freezable(), &mut done).unwrap_or(Ok(()))
            }
            Err(SleepError::Signal) => Ok(()),
        }
    }

    /* An entry's open: a thread asleep with nothing to reclaim has something */
    pub fn reclaim_wake(&self) {
        self.reclaim_thread.wake();
    }

    fn flush_pins_or_still_flushing(&self, reclaim: &mut ReclaimGuard<'_>, seq_to_flush: u64, types: u32) -> bool {
        self.flush_pins_locked(reclaim, seq_to_flush, types, 0, 0) != 0 ||
            self.pins().still_flushing(seq_to_flush, types)
    }

    fn flush_done(&self, seq_to_flush: u64, did_work: &mut bool) -> bool {
        if self.error().is_err() {
            return true;
        }

        let mut reclaim = self.reclaim_lock.lock();

        for ty in (0..JOURNAL_PIN_TYPE_NR as u32).rev() {
            if self.flush_pins_or_still_flushing(&mut reclaim, seq_to_flush, 1 << ty) {
                *did_work = true;

                /*
                 * Question from Dan Carpenter, on the early return:
                 *
                 * If journal_flush_pins_or_still_flushing() returns
                 * true, then the flush hasn't complete and we must
                 * return 0; we want the outer closure_wait_event() in
                 * journal_flush_pins() to continue.
                 *
                 * The early return is there because we don't want to
                 * cycle the journal until we've finished
                 * flushing all outstanding journal pins - otherwise
                 * seq_to_flush can be U64_MAX, and we'll close a bunch
                 * of journal entries and write tiny ones completely
                 * unnecessarily.
                 *
                 * Having the early return be in the loop where we loop
                 * over types is important, because flushing one journal
                 * pin can cause new journal pins to be added (even of
                 * the same type, btree node writes may generate more
                 * btree node writes, when updating the parent pointer
                 * has a full node and has to trigger a split/compact).
                 *
                 * This is part of our shutdown sequence, where order of
                 * flushing is important in order to make sure that it
                 * terminates...
                 */
                return false;
            }
        }

        if seq_to_flush >= self.fs().journal_cur_seq() {
            self.cycle(c::JournalCycleFlags::must_close);
        }

        /*
         * If journal replay hasn't completed, the unreplayed journal entries
         * hold refs on their corresponding sequence numbers
         */
        let _j = self.lock.lock();
        let last_seq = self.lock.last_seq.get();
        !self.flags.get(Journal::replay_done) ||
            last_seq > seq_to_flush ||
            last_seq == self.pin_range().pin_back.get()
    }

    /// The pins up to @seq flushed: whether any were.
    pub fn flush_pins(&self, seq_to_flush: u64) -> bool {
        /* time_stats this */
        let mut did_work = false;

        if !self.flags.get(Journal::running) {
            return false;
        }

        closure_wait_event(&self.reclaim_flush_wait, || self.flush_done(seq_to_flush, &mut did_work));

        did_work
    }

    pub fn flush_device_pins(&self, dev_idx: c_int) -> Result<(), BchError> {
        let fs = self.fs();

        let seq = {
            let pins = self.pins();
            let _j = self.lock.lock();
            pins.last_dev_seq(dev_idx, fs.journal_replicas())
        };

        self.flush_pins(seq);

        self.error()?;

        /*
         * flush_pins() only advances the in-memory last_seq past @seq. The
         * device's journal replicas entries aren't dropped until last_seq_ondisk
         * advances past them (in journal_write_done()), and that requires a
         * journal write recording the new last_seq. Force one here so we don't
         * depend on a caller flushing afterwards: otherwise dev_idx lingers in
         * the on-disk journal replicas set, and once it's taken offline the next
         * superblock write can't satisfy that entry and goes emergency read-only.
         */
        fs.journal_meta()
    }

    /*
     * Before a device leaves the journal write set (going RO/evacuating), push
     * journal reclaim until the *other* journal devices have free space to write
     * to. Otherwise, if @dev_idx held the journal's only free space, dropping it
     * from rw_devs[journal] strands the journal: reclaim can no longer advance
     * last_seq_ondisk (which needs a journal write, hence a writable device) to
     * free anyone else's buckets, and we deadlock in journal_full - with the task
     * taking the device offline wedged in journal_res_get while holding state_lock.
     *
     * Must run before @dev_idx is pulled from rw_devs[journal], so reclaim can
     * still use it to write the last_seq advance that frees the remaining devices.
     * Can't fail: the only way out of the loop besides success is the whole
     * filesystem going read-only, which stops reclaim.
     */
    pub fn flush_dev_ro(&self, dev_idx: u32) {
        let fs = self.fs();
        let want = fs.journal_replicas() as usize;

        loop {
            let (nr_devs, nr_with_space, flush_needed) = {
                let g = self.lock.lock();
                let (mut nr_devs, mut nr_with_space) = (0, 0);

                {
                    let rcu = read_lock();
                    for ca in fs.rw_devs_for_rcu(DataType::journal, &rcu) {
                        if u32::from(ca.dev_idx) == dev_idx || ca.journal.nr() == 0 {
                            continue;
                        }
                        nr_devs += 1;
                        if ca.journal_ring(&g).available(journal_space_discarded) != 0 {
                            nr_with_space += 1;
                        }
                    }
                }

                /*
                 * Reclaim advances last_seq (in memory) as it flushes
                 * pins, but the buckets it frees can't be discarded until
                 * last_seq_ondisk catches up, which only happens when a
                 * journal write completes (journal_write_done()). Only
                 * force a write when there's such an advance to persist.
                 */
                (nr_devs, nr_with_space, self.lock.last_seq_ondisk.get() < self.lock.last_seq.get())
            };

            /*
             * Enough other devices have free journal space that the journal
             * can keep writing once @dev_idx leaves the write set:
             */
            if nr_with_space >= want.min(nr_devs) {
                break;
            }

            let ret = if flush_needed {
                fs.journal_meta()
            } else {
                self.reclaim(&mut self.reclaim_lock.lock())
            };
            if ret.is_err() {
                /* filesystem read-only */
                break;
            }
        }
    }

    #[cold]
    pub fn pins_to_text(&self, out: &mut Printbuf, limit: u32) {
        let mut seq = 0;
        let mut nr = 0;

        /* limit the number of pins printed, not the number of seqs scanned */
        while nr < limit && !self.seq_pins_to_text(out, &mut seq, Some(&mut nr)) {
            seq += 1;
        }
    }

    #[cold]
    pub fn reclaim_to_text(&self, out: &mut Printbuf) {
        use c::TimeStat as Time;
        let fs = self.fs();
        let now = jiffies();

        if out.nr_tabstops() == 0 {
            out.tabstop_push(36);
        }

        prt!(out, "nr direct reclaim:\t{}\n", self.reclaim_lock.nr_direct_reclaim.get());
        prt!(out, "nr background reclaim:\t{}\n", self.reclaim_lock.nr_background_reclaim.get());
        if let Some(kicks) = self.reclaim_thread.kicks() {
            prt!(out, "reclaim kicks:\t{kicks}\n");
        }
        prt!(out, "last flushed:\t{} ms ago\n", jiffies_to_msecs(now.wrapping_sub(self.reclaim_lock.last_flushed.get())));
        let next = self.next_reclaim.get();
        prt!(out, "next reclaim:\t{} ms\n",
             if time_after(next, now) { jiffies_to_msecs(next.wrapping_sub(now)) } else { 0 });

        self.flush_in_progress_to_text(out);

        out.tabstops_reset();
        out.tabstop_push(24);
        out.tabstop_push(12);
        out.tabstop_push(12);

        out.newline();
        prt!(out, "Pin flush time stats:\tcount\tavg\tmax\n");
        for (name, t) in [("  btree",     Time::journal_pin_flush_btree),
                          ("  key_cache", Time::journal_pin_flush_key_cache),
                          ("  other",     Time::journal_pin_flush_other)] {
            time_stats_summary_to_text(out, name, &fs.times[t as usize]);
        }

        out.newline();
        prt!(out, "Blocked time stats:\tcount\tavg\tmax\n");
        for (name, t) in [("  low_on_space",        Time::blocked_journal_low_on_space),
                          ("  low_on_pin",          Time::blocked_journal_low_on_pin),
                          ("  low_on_open_buckets", Time::blocked_journal_low_on_open_buckets),
                          ("  max_in_flight",       Time::blocked_journal_max_in_flight),
                          ("  max_open",            Time::blocked_journal_max_open),
                          ("  write_buffer_full",   Time::blocked_write_buffer_full)] {
            time_stats_summary_to_text(out, name, &fs.times[t as usize]);
        }

        out.newline();
        prt!(out, "Oldest journal pins:\n");
        self.pins_to_text(out, 2);

        out.newline();

        if let Some(task) = self.reclaim_thread.task() {
            prt!(out, "Reclaim thread:\n");
            let _ = prt_task_backtrace(&mut out.indent(2), task.as_ptr(), 0);
        } else {
            prt!(out, "Reclaim thread not running\n");
        }
    }
}

#[cold]
fn time_stats_summary_to_text(out: &mut Printbuf, name: &str, stats: &c::bch2_time_stats) {
    let (n, mean, max) = {
        let s = stats.lock.lock();
        (s.duration_stats.n, s.duration_stats.mean(), s.max_duration)
    };

    prt!(out, "{name}:\t{n}\t{}\t{}\n", Nsecs(mean as u64), Nsecs(max));
}

/*
 * Advance the discard cursor as long as it points to buckets that are no longer
 * dirty, issuing discards if necessary:
 */
pub(crate) fn discard_work(ja: &c::journal_device) {
    let ca = ja.container();
    ca.fs().journal.dev_do_discards(ca);
}

fn reclaim_thread(j: &c::journal, me: &KthreadSelf<'_>) {
    let fs = j.fs();

    Kthread::set_freezable();

    let _ = sleep_until(Sleep::INTERRUPTIBLE.freezable(),
        || (fs.flags.get(c::FsFlag::rw) || Kthread::should_stop()).then_some(()));

    j.reclaim_lock.lock().published().last_flushed.set(jiffies());

    let mut kicks = me.kicks();
    let mut ret = Ok(());

    while ret.is_ok() && !Kthread::should_stop() {
        let now_kicks = me.kicks();
        let kicked = now_kicks != kicks;
        kicks = now_kicks;

        ret = j.reclaim_pass(&mut j.reclaim_lock.lock(), false, kicked);

        let now = jiffies();
        let delay = msecs_to_jiffies(fs.opts().journal_reclaim_delay.get().into());
        let mut next = j.reclaim_lock.last_flushed.get().wrapping_add(delay);

        if !time_in_range(next, now, now.wrapping_add(delay)) {
            next = now.wrapping_add(delay);
        }
        j.next_reclaim.set(next);

        reclaim_sleep(j, me, kicks);
    }
}

/*
 * Until it's kicked, or stopped, or next_reclaim - with no timeout while the
 * journal's empty: whatever opens an entry wakes it
 */
fn reclaim_sleep(j: &c::journal, me: &KthreadSelf<'_>, kicks: u32) {
    /* unlocked: front and back read apart, so compared, not subtracted */
    let empty_now = || {
        let r = j.pin_range();
        r.pin_front.get() == r.pin_back.get()
    };

    while !Kthread::should_stop() && !me.kicked_since(kicks) {
        let journal_empty = {
            let _j = j.lock.lock();
            j.pin_range().used() == 0
        };
        let timeout = j.next_reclaim.get().wrapping_sub(jiffies()) as isize;

        let how = Sleep::INTERRUPTIBLE.freezable();
        let how = match (journal_empty, timeout) {
            (true, _)           => how,
            (false, t) if t > 0 => how.timeout(t as _),
            (false, _)          => return,
        };

        let _ = sleep_until(how, || {
            (Kthread::should_stop() || me.kicked_since(kicks) || empty_now() != journal_empty).then_some(())
        });
    }
}

