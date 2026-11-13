// SPDX-License-Identifier: GPL-2.0

//! Journal writes: a journal buf's entry compacted, its devices allocated,
//! checksummed and submitted - a preflush first, if its flush goes
//! separately - and, once written, completed in seq order.

use core::mem::{self, offset_of};
use core::ptr::{self, NonNull};
use core::sync::atomic::Ordering::{Relaxed, SeqCst};

use crate::alloc::disk_groups::Target;
use crate::alloc::replicas::DevlistEntry;
use crate::btree::bkey::prelude::*;
use crate::btree::bkey::{BkeySC, ValidateFlags};
use crate::btree::interior::btree_roots_to_journal_entries;
use crate::btree::write_buffer;
use crate::c;
use crate::c::JournalBucketStage::JOURNAL_BUCKET_cur;
use crate::c::JournalFlag as Journal;
use crate::c::JournalSpaceFrom::journal_space_discarded;
use crate::c::{buf_waiters, JournalGuard, JOURNAL_BUF_FLUSH, JOURNAL_BUF_FLUSH_NO_WAIT, JOURNAL_BUF_NOFLUSH,
               JOURNAL_BUF_NOT_IN_FLIGHT};
use crate::data::ptrs::{BkeyVPtrs, Ptrs};
use crate::data::write::DevWriteRef;
use crate::data_structures::array_vec::ArrayVec;
use crate::debug::trace::events;
use crate::errcode::{Errcode, BchError};
use crate::format::DataType;
use crate::fs::{Dev, Fs};
use crate::journal::entries::EntryWriter;
use crate::journal::pins::PinsRead;
use crate::journal::validate::jset_validate;
use crate::locking::MemallocFlags;
use crate::sb::members::{DevIo, DevIoRef};
use crate::util::bio::plugged;
use crate::util::closure::Running;
use crate::os::kernel::{jiffies, ktime_get_real_seconds, local_clock, msecs_to_jiffies, time_after_eq, unlikely,
                          WRITE};
use crate::util::rcu::read_lock;
use crate::util::time_stats::TimeStats;
use crate::util::vstructs::{cast_plain_slice_mut, Head, SelfSized, Unvalidated};
use crate::util::printbuf::ToText;
use crate::{log_msg_ratelimited, prt};

/* IOPRIO_PRIO_VALUE(IOPRIO_CLASS_RT, 0) */
const IOPRIO_RT: u16 = 1 << 13;

crate::has_closure!(c::journal_buf, io);

impl c::journal_buf {
    fn head(&self) -> &Head<c::jset> {
        self.data.head()
    }

    fn head_mut(&mut self) -> &mut Head<c::jset> {
        self.data.head_mut()
    }

    fn jset(&self) -> &c::jset<Unvalidated> {
        self.data.jset()
    }

    fn jset_mut(&mut self) -> &mut c::jset<Unvalidated> {
        self.data.jset_mut()
    }

    fn seq(&self) -> u64 {
        self.head().seq.get()
    }

    /* The devices it's allocated, a pointer each: bkey_val_u64s(&w->key.k) */
    fn nr_ptrs(&self) -> usize {
        self.key.k.val_u64s()
    }

    fn jset_and_tail(&mut self) -> (&mut [u8], &mut [u8]) {
        let bytes = self.jset().bytes();
        self.data.bytes_mut().split_at_mut(bytes)
    }

    /// Entries appended to its jset, by @f: the u64s they took.
    fn append_entries(&mut self, f: impl FnOnce(&mut EntryWriter<'_>)) -> usize {
        let (_, tail) = self.jset_and_tail();
        let mut ew = EntryWriter::new(cast_plain_slice_mut(tail).expect("a journal buf, u64s past its jset"));
        f(&mut ew);
        let u64s = ew.len();

        let h = self.head_mut();
        h.set_u64s(h.u64s() + u64s);
        u64s
    }

    pub fn must_not_flush(&self) -> bool {
        self.wait_word().load(Relaxed) == JOURNAL_BUF_NOFLUSH
    }

    /* Decided a flush - false if try_noflush() got there first: */
    fn try_flush(&self) -> bool {
        match self.wait_word().compare_exchange(ptr::null_mut(), JOURNAL_BUF_FLUSH, SeqCst, Relaxed) {
            Ok(_) => true,
            Err(old) => old != JOURNAL_BUF_NOFLUSH,
        }
    }
}

impl c::JournalLocked {
    fn last_unallocated_seq(&self) -> u64 {
        (self.in_flight.front_idx()..)
            .zip(self.in_flight.iter())
            .find(|(_, buf)| !buf.bits.write_allocated())
            .map_or(0, |(seq, _)| seq)
    }
}

fn advance_devs_to_next_bucket(j: &c::journal, devs: &c::bch_devs_mask, sectors: u32, seq: u64) {
    let fs = j.fs();
    let rcu = read_lock();
    let mut g = j.lock.lock();

    for i in devs.d.iter() {
        let Some(ca) = fs.dev_rcu(i as u32, &rcu) else { continue };
        let ja = &ca.journal;
        let bucket_size: u32 = ca.mi.get().bucket_size.into();

        if sectors > ja.sectors_free.get() &&
            sectors <= bucket_size &&
            ca.journal_ring(&g).available(journal_space_discarded) != 0 {
            /* the new cur bucket's seq must always be something sensible: */
            ca.journal_ring_mut(&mut g).next_bucket(seq);
            ja.sectors_free.set(bucket_size);
        }
    }
}

/*
 * Each new set of journal devices is a new replicas entry, a superblock write:
 * stick with the last write's devices unless another is this much faster.
 */
const JOURNAL_DEV_STICKINESS: u64 = 8;

fn dev_score(a: &c::WriteAlloc, ca: &Dev) -> u64 {
    let latency = ca.cur_latency[WRITE].load(Relaxed) as u64 + 1;

    if a.wp_prev_devs.d.test(ca.dev_idx.into()) {
        latency
    } else {
        latency * JOURNAL_DEV_STICKINESS
    }
}

fn __write_alloc(j: &c::journal, a: &mut c::WriteAlloc, w: &mut c::journal_buf, devs: &c::bch_devs_mask,
                 sectors: u32, replicas: &mut u32, replicas_want: u32) {
    let fs = j.fs();

    while *replicas < replicas_want {
        /*
         * Journal replicas spread across failure domains, like data:
         * one pick per sort, since each pick changes the occupancy
         * the next should avoid:
         */
        let mut chosen = c::bch_devs_mask::default();
        if let Some(k) = BkeySC::from(w.key.key()).arm::<Ptrs>() {
            for ptr in k.ptrs(fs) {
                chosen.d.set(ptr.dev().into());
            }
        }

        let mut devs_sorted = c::dev_alloc_list::default();
        fs.dev_alloc_list_devs(&mut a.stripe, devs, &chosen, &mut a.wp_domain_keys, &mut devs_sorted);

        /* The lowest score in the best failure domain, ties in sort order: */
        let mut ca: Option<DevIoRef<c::DevWriteRefKind>> = None;
        for &i in &devs_sorted.data[..devs_sorted.nr as usize] {
            if ca.as_ref().is_some_and(|ca| a.wp_domain_keys[usize::from(i)] != a.wp_domain_keys[usize::from(ca.dev_idx)]) {
                break;
            }

            let Some(cand) = fs.dev_get_ioref::<c::DevWriteRefKind>(i.into()) else { continue };
            let ja = &cand.journal;

            /*
             * Check that we can use this device, and aren't
             * already using it:
             */
            if ja.nr() == 0 ||
                BkeySC::from(w.key.key()).arm::<Ptrs>().is_some_and(|k| k.has_device(fs, cand.dev_idx.into()).is_some()) ||
                sectors > ja.sectors_free.get() ||
                ca.as_ref().is_some_and(|ca| dev_score(a, &cand) >= dev_score(a, ca)) {
                continue;
            }

            ca = Some(cand);
        }

        let Some(ca) = ca else { return };
        let ja = &ca.journal;
        let bucket_size: u32 = ca.mi.get().bucket_size.into();

        a.stripe.increment(&ca, &ca.usage_read_fast());

        let bucket = {
            let mut g = j.lock.lock();
            let bucket = ca.journal_ring(&g).bucket(JOURNAL_BUCKET_cur);
            ca.journal_ring_mut(&mut g).set_cur_seq(w.seq());
            bucket
        };

        let offset = ca.bucket_to_sector(bucket) + u64::from(bucket_size) - u64::from(ja.sectors_free.get());

        /*
         * Checked here, not at submit: flush writes are submitted after
         * their preflushes, so behind later noflush writes - a device's
         * submissions aren't in allocation order, and a ring lap can
         * make two consecutive ones the same sector, legitimately:
         */
        assert!(offset != ca.prev_journal_sector.get(), "journal write to {}:{offset} again", ca.dev_idx);
        ca.prev_journal_sector.set(offset);

        crate::data::extents::bkey_append_ptr(&mut w.key.room(), c::bch_extent_ptr::new().with_offset(offset).with_dev(ca.dev_idx))
            .expect("a journal key's pointers fit in key_pad");

        /* Stash ca alongside the just-appended ptr; submit + no_io
         * walk @key.k.u64s ptrs in order, so the index lines up. */
        let idx = w.nr_ptrs() - 1;
        ja.sectors_free.set(ja.sectors_free.get() - sectors);
        *replicas += u32::from(ca.mi.get().durability);
        w.cas[idx] = Some(ca);
    }
}

fn write_alloc(j: &c::journal, w: &mut c::journal_buf, replicas: &mut u32) -> Result<(), BchError> {
    let fs = j.fs();
    let sectors = w.jset().sectors(fs.block_bits()) as u32;
    let mut target = match fs.opts().metadata_target.get() {
        0 => fs.opts().foreground_target.get(),
        t => t,
    };
    let replicas_want = fs.journal_replicas();
    let seq = w.seq();
    let a = unsafe { j.write_alloc.protected_mut() };

    'target: loop {
        let devs = fs.target_rw_devs(DataType::journal, Target::decode(target.into()));

        /* The last write's devices, if out of room, move to their next bucket: */
        let mut prev = devs;
        prev.d.and(&a.wp_prev_devs.d);
        advance_devs_to_next_bucket(j, &prev, sectors, seq);

        __write_alloc(j, a, w, &devs, sectors, replicas, replicas_want);
        if *replicas >= replicas_want {
            break;
        }

        advance_devs_to_next_bucket(j, &devs, sectors, seq);
        __write_alloc(j, a, w, &devs, sectors, replicas, replicas_want);
        if *replicas >= replicas_want {
            break;
        }

        if target != 0 {
            /* Retry from all devices: */
            target = 0;
            continue 'target;
        }
        break;
    }

    assert!(w.nr_ptrs() <= c::BCH_REPLICAS_MAX as usize, "journal write to {} devices", w.nr_ptrs());

    if *replicas != 0 {
        a.wp_prev_devs = c::bch_devs_mask::default();
        if let Some(k) = BkeySC::from(w.key.key()).arm::<Ptrs>() {
            for ptr in k.ptrs(fs) {
                a.wp_prev_devs.d.set(ptr.dev().into());
            }
        }
    }

    if *replicas != 0 {
        Ok(())
    } else {
        Err(fs.err(Errcode::BCH_ERR_insufficient_journal_devices))
    }
}

fn buf_realloc(j: &c::journal, buf: &mut c::journal_buf) {
    let fs = j.fs();

    /* we aren't holding j->lock: */
    let new_size = j.lock.buf_size_want.get();

    if buf.data.size() >= new_size {
        return;
    }

    let btree_write_buffer_size = new_size as usize / 64;

    if fs.btree_write_buffer_resize(btree_write_buffer_size).is_err() {
        return;
    }

    /*
     * Speculative growth, and every failure path here just declines to
     * grow - but we're under j->buf_lock, which journal_write_done() needs
     * as its first act, so sitting in reclaim for a bigger buffer stalls
     * journal write completions. Take the failure immediately instead.
     */
    let Ok(mut new_buf) = c::JsetBuf::new(new_size, c::GFP_NOIO | c::__GFP_NORETRY | c::__GFP_NOWARN) else {
        return;
    };

    new_buf.bytes_mut()[..buf.data.size() as usize].copy_from_slice(buf.data.bytes());

    {
        let _g = j.lock.lock();
        mem::swap(&mut buf.data, &mut new_buf);
    }

    drop(new_buf);
}

/*
 * The replicas entries of the seqs last_seq_ondisk is advancing past, with a
 * count each, to put once j->lock is dropped: a fixed number, so they can be
 * gathered under the lock - more than that, and the caller puts these and
 * comes back for the rest.
 */
type ReplicasRefs = ArrayVec<(DevlistEntry, u32), 16>;

fn replicas_refs_put(fs: &Fs, refs: &mut ReplicasRefs) {
    for (r, nr) in refs.iter() {
        fs.replicas_entry_put_many(r, *nr);
    }
    refs.clear();
}

fn update_last_seq_ondisk(j: &c::journal, pins: &PinsRead<'_>, g: &mut JournalGuard<'_>, last_seq_ondisk: u64,
                          refs: &mut ReplicasRefs) -> Result<(), BchError> {
    assert!(last_seq_ondisk <= j.pin_range().pin_back.get(),
            "journal last_seq_ondisk {last_seq_ondisk} past pin_back {}", j.pin_range().pin_back.get());

    for seq in j.lock.last_seq_ondisk.get()..last_seq_ondisk {
        let devs = pins.seq_devs(seq);

        if devs.nr != 0 {
            let devs_r = DevlistEntry::new(DataType::journal, devs);

            match refs.iter_mut().find(|(r, _)| *r == devs_r) {
                Some((_, nr)) => *nr += 1,
                None => refs.try_push((devs_r, 1)).map_err(|_| j.fs().err(Errcode::BCH_ERR_ENOMEM_journal_buf))?,
            }

            pins.seq_set_devs(seq, &c::bch_devs_list::default());
        }

        let mut bytes = pins.seq_take_bytes(seq) as usize;
        if crate::warn_on!(g.dirty_entry_bytes < bytes) {
            bytes = g.dirty_entry_bytes;
        }

        g.dirty_entry_bytes -= bytes;
    }

    Ok(())
}

/*
 * write_done has two-state semantics that matter for concurrent
 * journal_write_done() callbacks:
 *
 *   write_done == false: bio for this seq has completed, but post-completion
 *	bookkeeping (replicas_refs_put, last_seq_ondisk / flushed_seq_ondisk
 *	advance) has NOT yet finished, OR we haven't entered the callback for
 *	this seq yet.
 *   write_done == true:  ALL post-completion bookkeeping is finished. Safe
 *	for an unrelated thread to treat the seq as fully flushed.
 *
 * The completion loop in journal_write_done() is allowed to drive seqs
 * forward in two cases:
 *
 *   - write_done is set (a prior callback already finished the bookkeeping
 *     and left this seq queued for a later callback to pop), or
 *   - seq == seq_completing (we are the callback for this seq, so we own
 *     its bookkeeping for this iteration of the loop, even though write_done
 *     isn't set yet - it gets set only after the loop finishes).
 *
 * Setting write_done up front would collapse the distinction: a concurrent
 * journal_write_done() callback could observe write_done == true during
 * our lock-drop window for replicas_refs_put, advance flushed_seq_ondisk
 * past our seq, and let going-RO mark the FS clean before our refs are
 * actually put.
 */
fn last_uncompleted_write_seq(g: &JournalGuard<'_>, seq_completing: u64) -> u64 {
    let Some(front) = g.in_flight.peek_front() else { return 0 };
    let seq = g.in_flight.front_idx();

    if front.bits.write_done() || seq == seq_completing { seq } else { 0 }
}

extern "C" fn write_done(mut w: Running<c::journal_buf>) {
    let j = w.journal();
    let fs = j.fs();
    let seq_wrote = w.seq();
    let mut err = Ok(());

    let no_flush = w.head().no_flush();
    let time = if no_flush {
        c::TimeStat::journal_noflush_write
    } else {
        c::TimeStat::journal_flush_write
    };
    fs.times[time as usize].update(j.write_start_time.get());

    /*
     * j->buf_lock guards journal buf data lifetime against the btree write
     * buffer flush path: fetch_wb_keys_from_journal() walks buf->data with
     * buf_lock held, and on the error path (emergency shutdown) we can get
     * here with need_flush_to_write_buffer still set - i.e. with a flush
     * concurrently reading the buf we're about to free.
     *
     * Lock ordering: buf_lock, pin_resize_lock, then j->lock - same as the
     * flusher and the journal write path.
     */

    /*
     * kvfree can sleep, so it can't run under j->lock (or any lock here):
     * buf_to_free is dropped after every lock is.
     */
    let buf_to_free;

    let b = j.buf_lock.lock();

    /*
     * pin_resize_lock held across the journal pin FIFO updates below; it
     * keeps the pin_list stable while we rebuild temp replicas entries from
     * the compact device list for refcount updates.
     */
    let mut pins = j.pins();
    let write_failed = !w.failed.is_empty();
    let flush_failed = !w.flush_failed.is_empty();

    if unlikely(write_failed || flush_failed) {
        fs.replicas_entry_put_many(&DevlistEntry::new(DataType::journal, pins.seq_devs(seq_wrote)), 1);
        pins.seq_set_devs(seq_wrote, &c::bch_devs_list::default());
    }

    if pins.seq_devs(seq_wrote).nr == 0 && !w.bits.empty() {
        err = fs.replicas_entry_get(&DevlistEntry::new(DataType::journal, w.devs_written));
        if err.is_ok() {
            pins.seq_set_devs(seq_wrote, &w.devs_written);
        }
    }

    if unlikely(write_failed || flush_failed || err.is_err()) {
        if w.devs_written.nr == 0 {
            err = Err(fs.err(Errcode::BCH_ERR_journal_write_err));
        }

        /*
         * Writing degraded because a device was removed is not
         * news: the removal was reported when it happened, and
         * reconcile restores the replicas.
         *
         * After the write_err throw above, not before: with every
         * device removed and nothing written this entry has failed,
         * and that must still be said - we're going emergency
         * read-only on it.
         */
        let mut msg = if err.is_ok() && w.failed.all_dev_removed() && w.flush_failed.all_dev_removed() {
            let mut msg = crate::util::log::LogMsg::new(fs);
            msg.set_suppressed(true);
            msg
        } else {
            /* Separate ratelimit_states for hard and soft errors */
            log_msg_ratelimited!(fs, err.is_err() as usize, 2)
        };

        prt!(&mut *msg, "error writing journal entry {seq_wrote}\n");
        w.failed.to_text(&mut msg, fs);
        if flush_failed {
            prt!(&mut *msg, "preflush errors:\n");
            w.flush_failed.to_text(&mut msg, fs);
        }

        match err {
            Ok(()) => {
                if w.devs_written.nr < BkeySC::from(w.key.key()).arm::<Ptrs>().map_or(0, |k| k.devs(fs).nr) {
                    prt!(&mut *msg, "wrote degraded to ");
                    fs.devs_list_to_text(&mut msg, &w.devs_written);
                    msg.newline();
                }
            }
            Err(e) => {
                prt!(&mut *msg, "error {}\n", e.msg());
                drop(pins);
                fs.emergency_read_only(&mut msg);
                pins = j.pins();
            }
        }
    }

    let mut g = j.lock.lock();
    assert!(seq_wrote >= j.pin_range().pin_front.get(),
            "journal write of seq {seq_wrote} done, behind pin_front {}", j.pin_range().pin_front.get());
    if err.is_err() && (j.lock.err_seq.get() == 0 || seq_wrote < j.lock.err_seq.get()) {
        g.published().err_seq.set(seq_wrote);
    }

    g.flushes_outstanding -= u32::from(!no_flush);

    if g.free_buf.size() < w.data.size() {
        mem::swap(&mut g.free_buf, &mut w.data);
    }

    buf_to_free = mem::take(&mut w.data);
    drop(b);
    let _ = w.end();

    let (mut g, done) = complete_writes(j, g, &pins, seq_wrote);

    /*
     * Mark our buffer's bookkeeping done. Two cases:
     *
     *  - complete_writes() popped past us (front advanced beyond seq_wrote):
     *    seq_to_buf() returns None, nothing to do - the pop already
     *    obviated the need.
     *  - It stopped before reaching us (some earlier seq is still
     *    in flight): we're still in the FIFO, mark write_done so a later
     *    journal_write_done() callback can drive its loop through our seq.
     *
     * Must come after it: write_done is the "all bookkeeping done"
     * signal. Setting it before opens a race where a concurrent
     * callback observes us as fully done during our lock-drop window for
     * replicas_refs_put.
     */
    if let Some(w) = g.seq_to_buf_mut(seq_wrote) {
        w.bits.set_write_done(true);
    }

    pins.advance_front(j.lock.last_seq_ondisk.get());

    if done.any {
        /*
         * Updating last_seq_ondisk may let bch2_journal_reclaim_work() discard
         * more buckets:
         *
         * Must come before signaling write completion, for
         * journal.stop():
         */
        if j.watermark() != c::Watermark::stripe {
            j.reclaim_kick();
        }

        j.update_last_seq(&pins, &mut g);
        j.space_available(&mut g);

        fs.times[c::TimeStat::blocked_journal_max_in_flight as usize].track_event_change(false);

        j.wake_async_wait();
    }

    /*
     * If the pipeline's drained and the open entry has a flush requested
     * (e.g. cascaded onto it from an earlier entry we demoted to noflush),
     * close it now so it gets written as the next flush - we don't close it
     * to write it while there are previous entries still in flight:
     *
     * Barrier needed between incrementing j->in_flight.front and checking
     * for waiters:
     */
    core::sync::atomic::fence(SeqCst);

    let _ = j.cycle_locked(&pins, &mut g, c::JournalCycleFlags::empty());

    /*
     * We don't typically trigger journal writes from here - the next journal
     * write will be triggered immediately after the previous one is
     * allocated, in bch2_journal_write() - but the journal write error path
     * is special:
     */
    j.do_writes_locked(&mut g);
    drop(g);
    drop(pins);
    drop(buf_to_free);

    if done.last_seq_ondisk_moved {
        fs.reset_alloc_cursors();
        fs.do_discards_async();
    }

    unsafe { c::closure_put(&raw const fs.cl as *mut _) };
}

struct Completed {
    any:                   bool,
    last_seq_ondisk_moved: bool,
}

/*
 * Every write done in seq order from the front, @seq_wrote's included if the
 * ones before it are: what's on disk advanced past each - and for those not
 * noflush, last_seq_ondisk and flushed_seq_ondisk - its waiters woken and its
 * buf retired. j->lock is dropped to put the replicas refs last_seq_ondisk
 * releases, so the guard is handed back.
 */
fn complete_writes<'a>(j: &'a c::journal, mut g: JournalGuard<'a>, pins: &PinsRead<'_>, seq_wrote: u64)
    -> (JournalGuard<'a>, Completed)
{
    let fs = j.fs();
    let mut replicas_refs = ReplicasRefs::new();
    let mut done = Completed { any: false, last_seq_ondisk_moved: false };

    loop {
        let seq = last_uncompleted_write_seq(&g, seq_wrote);
        if seq == 0 {
            break;
        }

        let (last_seq, empty, must_not_flush) = {
            let w = g.seq_to_buf(seq).expect("an in-flight journal buf");
            (w.last_seq, w.bits.empty(), w.must_not_flush())
        };

        if j.lock.err_seq.get() == 0 && !must_not_flush {
            assert!(!empty || last_seq == seq, "empty journal entry {seq} with last_seq {last_seq}");

            if j.lock.last_seq_ondisk.get() < last_seq {
                let _ = update_last_seq_ondisk(j, pins, &mut g, last_seq + u64::from(empty), &mut replicas_refs);
                /*
                 * update_last_seq_ondisk() can return an error if
                 * appending to replicas_refs failed, but we don't
                 * care - it's preallocated, so it'll always be able
                 * to do some work, and we have to retry anyways,
                 * because we have to drop j->lock to put the
                 * replicas refs before updating j->flushed_seq_ondisk
                 */

                /*
                 * Do this before updating j->last_seq_ondisk,
                 * or journal flushing breaks:
                 */
                if !replicas_refs.is_empty() {
                    drop(g);
                    replicas_refs_put(fs, &mut replicas_refs);
                    g = j.lock.lock();
                    continue;
                }

                assert!(last_seq <= j.lock.last_seq.get(),
                        "journal entry {seq}'s last_seq {last_seq} past last_seq {}", j.lock.last_seq.get());
                g.published().last_seq_ondisk.set(last_seq);
                done.last_seq_ondisk_moved = true;
            }

            /* replicas refs need to be put first */
            let p = g.published();
            p.flushed_seq_ondisk.set(seq);
            p.rewind_seq_ondisk.set(j.lock.rewind_seq.get());
        }

        if empty {
            g.last_empty_seq = seq;
        }
        g.published().seq_ondisk.set(seq);

        if let Some(waiters) = g.seq_to_buf(seq).expect("an in-flight journal buf").take_waiters(JOURNAL_BUF_NOT_IN_FLIGHT) {
            waiters.wake();
        }

        done.any = true;

        /*
         * Advance the in_flight FIFO front. Maintains the invariant
         * fifo.front == seq_ondisk + 1 so seq_to_buf()'s indexing stays
         * consistent with seq. The buf is the FIFO's slot, retired in place;
         * buf->data was already recycled/freed above.
         */
        assert!(g.in_flight.front_idx() == seq, "journal completing seq {seq}, in_flight front {}",
                g.in_flight.front_idx());
        g.in_flight.pop_front().expect("an in-flight journal buf").retire();
    }

    (g, done)
}

extern "C" fn write_done_flush(w: Running<c::journal_buf>) {
    let j = w.journal();

    /*
     * Wake up flush waiters early, if there wasn't an error:
     *
     * Flush writes wait for previous outstanding writes, so there's no
     * ordering concerns here (as journal_write_done normally has to
     * handle). Don't mark it as closed to new flush waiters yet, since
     * we're not taking j->lock and updating the various seq_ondisk fields
     * yet:
     */
    if w.failed.is_empty() &&
        w.flush_failed.is_empty() &&
        buf_waiters(w.wait_word().load(Relaxed)) {
        if let Some(waiters) = w.take_waiters(JOURNAL_BUF_FLUSH) {
            waiters.wake();
        }
    }

    w.continue_at_nobarrier(write_done, Some(&j.wq));
}

/// # Safety
/// @bio is a journal_bio.
unsafe fn jbio_of<'a>(bio: *mut c::bio) -> &'a mut c::journal_bio {
    unsafe { &mut *bio.byte_sub(offset_of!(c::journal_bio, bio)).cast::<c::journal_bio>() }
}

unsafe extern "C" fn write_endio(bio: *mut c::bio) {
    let jbio = unsafe { jbio_of(bio) };
    let ca = unsafe { &*jbio.ca };
    let w = unsafe { &mut *jbio.buf };
    let j = w.journal();
    let fs = j.fs();
    let status = jbio.bio.bi_status;

    ca.account_io_completion(c::MemberErrorType::write, jbio.submit_time, status == 0);
    ca.write_unflushed();

    if status != 0 {
        let _g = j.err_lock.lock();
        w.failed.set_blk_status(fs, ca.dev_idx.into(), status);
        w.devs_written.drop(ca.dev_idx);
    }

    unsafe { c::closure_put(&raw mut w.io) };
    drop(unsafe { DevWriteRef::adopt(DevIo::of(ca)) });
    unsafe { c::bio_put(bio) };
}

unsafe extern "C" fn write_preflush_endio(bio: *mut c::bio) {
    let jbio = unsafe { jbio_of(bio) };
    let ca = unsafe { &*jbio.ca };
    let w = unsafe { &mut *jbio.buf };
    let j = w.journal();
    let fs = j.fs();
    let status = jbio.bio.bi_status;

    ca.account_io_completion(c::MemberErrorType::write, jbio.submit_time, status == 0);

    if status != 0 {
        let _g = j.err_lock.lock();
        w.flush_failed.set_blk_status(fs, ca.dev_idx.into(), status);
        w.devs_written.drop(ca.dev_idx);
    }

    unsafe { c::closure_put(&raw mut w.io) };
    drop(unsafe { DevWriteRef::adopt(DevIo::of(ca)) });
    unsafe { c::bio_put(bio) };
}

unsafe fn closure_bio_submit(bio: *mut c::bio, w: &c::journal_buf) {
    unsafe {
        c::closure_get(ptr::from_ref(&w.io).cast_mut());
        c::submit_bio(bio);
    }
}

extern "C" fn write_submit(mut w: Running<c::journal_buf>) {
    let j = w.journal();
    let fs = j.fs();
    let sectors = w.jset().sectors(fs.block_bits()) as u32;
    let flush = !w.head().no_flush();
    let cas = mem::take(&mut w.cas);

    fs.event_inc_trace(events::journal_write, |out| {
        prt!(out, "seq {} flush {} sectors {sectors}\n", w.seq(), flush as u32);
        prt!(out, "{}", BkeySC::from(w.key.key()).to_text(fs));
    });

    plugged(|| {
        let ptrs: ArrayVec<u64, { c::BCH_REPLICAS_MAX as usize }> =
            BkeySC::from(w.key.key()).arm::<Ptrs>().map(|k| k.ptrs(fs).map(|p| p.offset()).collect()).unwrap_or_default();

        for (offset, ca) in ptrs.iter().zip(cas) {
            let ca = ca.expect("an io ref for each of the journal key's pointers");

            ca.io_done_add(WRITE, DataType::journal, sectors.into());

            let ja = &ca.journal;

            /*
             * blk-wbt.c throttles all writes except those that have both
             * REQ_SYNC and REQ_IDLE set...
             */
            let mut opf = c::req_op_REQ_OP_WRITE | c::REQ_SYNC | c::REQ_IDLE | c::REQ_META;
            if flush {
                opf |= c::REQ_FUA;
            }
            if flush && !w.bits.separate_flush() {
                opf |= c::REQ_PREFLUSH;
            }

            /*
             * Large vmalloc'd journal buffers may exceed BIO_MAX_VECS
             * and need multiple bios chained together; physically
             * contiguous buffers fit in a single bvec and the helper
             * returns one bio.
             */
            let bio = unsafe {
                c::bch2_bio_map_and_chain(ca.disk_sb.bdev, w.data.as_ptr().cast(), (sectors << 9) as usize, *offset,
                                          opf, c::GFP_NOIO, ja.bio_set.as_ptr())
            };
            let jbio = unsafe { jbio_of(bio) };

            jbio.ca = ptr::from_ref::<Dev>(&ca).cast_mut();
            jbio.buf = ptr::from_ref::<c::journal_buf>(&w).cast_mut();
            jbio.submit_time = local_clock();

            jbio.bio.bi_end_io = Some(write_endio);
            jbio.bio.bi_private = ptr::from_ref::<Dev>(&ca).cast_mut().cast();
            jbio.bio.bi_ioprio = IOPRIO_RT;

            /* the bio's ref, which write_endio() puts: */
            ca.leak();
            unsafe { closure_bio_submit(bio, &w) };
        }
    });

    if flush {
        w.continue_at(write_done_flush, None);
    } else {
        w.continue_at(write_done, Some(&j.wq));
    }
}

extern "C" fn write_preflush(w: Running<c::journal_buf>) {
    let j = w.journal();
    let fs = j.fs();

    if w.bits.separate_flush() {
        for ca in fs.rw_members() {
            /*
             * A member with no completed writes since its last
             * flush has nothing for this flush to make durable:
             */
            if !ca.flags.swap(c::DevFlag::unflushed_writes, false) &&
                !w.devs_written.has(ca.dev_idx) {
                continue;
            }

            let ja = &ca.journal;
            let bio = unsafe {
                c::bio_alloc_bioset(ca.disk_sb.bdev, 0,
                                    c::req_op_REQ_OP_WRITE | c::REQ_SYNC | c::REQ_IDLE | c::REQ_META | c::REQ_PREFLUSH,
                                    c::GFP_NOIO, ja.bio_set.as_ptr())
            };
            let jbio = unsafe { jbio_of(bio) };

            jbio.ca = ptr::from_ref::<Dev>(&ca).cast_mut();
            jbio.buf = ptr::from_ref::<c::journal_buf>(&w).cast_mut();
            jbio.submit_time = local_clock();

            jbio.bio.bi_end_io = Some(write_preflush_endio);
            jbio.bio.bi_private = ptr::from_ref::<Dev>(&ca).cast_mut().cast();

            /* the bio's ref, which write_preflush_endio() puts: */
            ca.leak();
            unsafe { closure_bio_submit(bio, &w) };
        }

        w.continue_at(write_submit, Some(&j.wq));
    } else {
        /*
         * no need to punt to another work item if we're not waiting on
         * preflushes
         */
        write_submit(w);
    }
}

/// The entries write_prep() adds to every journal entry besides its btree
/// roots - the datetime, and super_entries_add_common()'s: what
/// super_entries_journal_res reserves.
pub(crate) const PREP_ENTRIES_U64S: usize =
    (size_of::<c::jset_entry_datetime>() + crate::sb::clean::COMMON_BYTES) / size_of::<u64>();

fn write_prep(j: &c::journal, w: &mut c::journal_buf) -> Result<(), BchError> {
    let fs = j.fs();
    let mut wb = None;
    let mut btree_roots_have = 0u64;
    let seq = w.seq();

    let mut empty = w.head().seq.get() == w.head().last_seq.get();

    if w.bits.need_flush_to_write_buffer() {
        wb = Some(write_buffer::Intake::start(fs, seq));

        /*
         * need_flush_to_write_buffer must be cleared under the write buffer's
         * locks, dropped by bch2_journal_keys_to_write_buffer_end(), to avoid
         * racing with write buffer flushing
         */
        let _g = j.lock.lock();
        w.bits.set_need_flush_to_write_buffer(false);
    }

    /*
     * Simple compaction, dropping empty jset_entries (from journal
     * reservations that weren't fully used) and merging jset_entries that
     * can be.
     *
     * If we wanted to be really fancy here, we could sort all the keys in
     * the jset and drop keys that were overwritten - probably not worth it:
     */
    for i in w.jset_mut().entries_mut() {
        /* jset_validate(), in write_checksum(), reports it: */
        let Ok(i) = i else { break };

        /* Empty entry: */
        if i.u64s() == 0 {
            continue;
        }

        let ty = c::JsetEntryType(i.type_.into());

        /* write buffer keys are retyped to btree keys below: */
        if ty == c::JsetEntryType::btree_keys ||
            ty == c::JsetEntryType::write_buffer_keys {
            empty = false;
        }

        /*
         * New btree roots are set by journalling them; when the journal
         * entry gets written we have to propagate them to
         * c->btree_roots
         *
         * But, every journal entry we write has to contain all the
         * btree roots (at least for now); so after we copy btree roots
         * to c->btree_roots we have to get any missing btree roots and
         * add them to this journal entry:
         */
        if ty == c::JsetEntryType::btree_root {
            fs.journal_entry_to_btree_root(i);
            btree_roots_have |= 1 << i.btree_id;
        } else if ty == c::JsetEntryType::write_buffer_keys {
            let intake = wb.as_mut().expect("write buffer keys in a journal entry not flushing to the write buffer");
            if let Err(e) = intake.entry(i) {
                crate::fs_fatal_error!(fs, "flushing journal keys to btree write buffer: {}", e.msg());
                let _ = wb.take().map(write_buffer::Intake::end);
                return Err(e);
            }
        }
    }

    if let Some(wb) = wb {
        if let Err(e) = wb.end() {
            crate::fs_fatal_error!(fs, "error flushing journal keys to btree write buffer: {}", e.msg());
            return Err(e);
        }
    }

    if empty {
        let _g = j.lock.lock();
        w.bits.set_empty(true);
    }

    let u64s = w.append_entries(|ew| {
        btree_roots_to_journal_entries(fs, ew, btree_roots_have).expect("root_journal_res's room");

        let mut d = c::jset_entry_datetime::default();
        d.entry.type_ = c::JsetEntryType::datetime.0 as u8;
        d.seconds.set(ktime_get_real_seconds() as u64);
        ew.push_entry(d).expect("super_entries_journal_res's room");

        crate::sb::clean::super_entries_add_common(fs, ew).expect("super_entries_journal_res's room");
    });

    crate::warn_on!(u64s > j.lock.entry_u64s_reserved.get() as usize);

    let sectors = w.jset().sectors(fs.block_bits()) as u32;

    if sectors > w.sectors {
        crate::fs_fatal_error!(fs, ": journal write overran available space, {} > {} (extra {u64s} reserved {}/{})",
                               w.jset().bytes(), w.sectors << 9, w.u64s_reserved, j.lock.entry_u64s_reserved.get());
        return Err(fs.err(Errcode::BCH_ERR_EINVAL_journal_write_overran_available_space));
    }

    Ok(())
}

fn write_checksum(j: &c::journal, w: &mut c::journal_buf) -> Result<(), BchError> {
    let fs = j.fs();
    let has_overwrites = w.bits.has_overwrites();
    let csum_type = fs.meta_checksum_type();
    let h = w.head_mut();
    let mut validate_before_checksum = false;

    h.magic.set(fs.jset_magic());
    h.version.set(fs.sb.version.get().into());

    h.set_big_endian(cfg!(target_endian = "big"));
    h.set_csum_type(csum_type);
    h.set_has_overwrites(has_overwrites);

    if csum_type.is_encryption() {
        validate_before_checksum = true;
    }

    if h.version.get() < c::bcachefs_metadata_version_current {
        validate_before_checksum = true;
    }

    if validate_before_checksum {
        jset_validate(fs, None, w.jset_and_tail().0, 0, ValidateFlags::write)?;
    }

    if let Err(e) = w.jset_mut().encrypt(fs) {
        crate::fs_fatal_error!(fs, "encrypting journal entry: {}", e.msg());
        return Err(e);
    }

    let csum = w.jset().checksum(fs);
    w.head_mut().csum = csum;

    if !validate_before_checksum {
        jset_validate(fs, None, w.jset_and_tail().0, 0, ValidateFlags::write)?;
    }

    let sectors = w.jset().sectors(fs.block_bits());
    let bytes = w.jset().bytes();
    w.data.bytes_mut()[bytes..sectors << 9].fill(0);
    Ok(())
}

extern "C" fn journal_write(mut w: Running<c::journal_buf>) {
    let j = w.journal();
    let fs = j.fs();
    let nr_rw_members = fs.allocator.rw_devs[DataType::free.0 as usize].weight();

    assert!(w.bits.write_started(), "journal write of seq {} not started", w.seq());
    assert!(!w.bits.write_allocated(), "journal write of seq {} already allocated", w.seq());
    assert!(!w.bits.write_done(), "journal write of seq {} already done", w.seq());

    let _noio = MemallocFlags::noio();

    j.write_start_time.set(local_clock());

    let ret = (|| -> Result<(), BchError> {
        {
            let _b = j.buf_lock.lock();
            buf_realloc(j, &mut w);
            write_prep(j, &mut w)?;
        }

        let ret = j.error();
        if ret.is_err() && j.flags.get(Journal::need_flush_write) {
            ret?;
        }

        let mut replicas_allocated = 0;
        let ret = loop {
            let ret = write_alloc(j, &mut w, &mut replicas_allocated);
            if ret.is_ok() || !j.lock.can_discard.get() {
                break ret;
            }

            j.do_discards();
        };
        ret?;

        write_checksum(j, &mut w)?;

        {
            let mut g = j.lock.lock();
            assert!(g.last_unallocated_seq() == w.seq(),
                    "journal writing seq {}, but the first unallocated is {}", w.seq(), g.last_unallocated_seq());

            /*
             * write is allocated, no longer need to account for it in
             * bch2_journal_space_available():
             */
            w.sectors = 0;
            w.bits.set_write_allocated(true);
            g.entry_bytes_written += w.jset().bytes() as u64;

            if nr_rw_members > 1 {
                w.bits.set_separate_flush(true);
            }

            /*
             * journal entry has been compacted and allocated, recalculate space
             * available:
             */
            j.space_available(&mut g);
            j.do_writes_locked(&mut g);
        }

        w.devs_written = BkeySC::from(w.key.key()).arm::<Ptrs>().map(|k| k.devs(fs)).unwrap_or_default();

        if w.wait_word().load(Relaxed) != JOURNAL_BUF_FLUSH_NO_WAIT {
            /*
             * Mark journal replicas before we submit the write to guarantee
             * recovery will find the journal entries after a crash.
             *
             * The clean->dirty transition entry (FLUSH_NO_WAIT) is the
             * exception: we defer this until after the write completes, so the
             * fs isn't marked dirty before its journal entry is on disk - and
             * flushers can't wait on that entry, so there's no one to wake
             * early ahead of the deferred superblock write.
             */
            let pins = j.pins();
            let seq = w.seq();

            if let Err(e) = fs.replicas_entry_get(&DevlistEntry::new(DataType::journal, w.devs_written)) {
                pins.seq_set_devs(seq, &c::bch_devs_list::default());
                return Err(e);
            }

            pins.seq_set_devs(seq, &w.devs_written);
        }

        Ok(())
    })();

    match ret {
        Ok(()) if fs.opts().nochanges.get() == 0 => {
            if !w.jset().no_flush() && w.bits.separate_flush() {
                return w.continue_at_nobarrier(write_preflush, None);
            } else {
                return w.continue_at_nobarrier(write_submit, None);
            }
        }
        Ok(()) => {}
        Err(e) => {
            let mut msg = crate::util::log::LogMsg::new(fs);
            msg.set_suppressed(true); /* only print once, when we go ERO */

            prt!(&mut *msg, "Unable to do journal write at seq {} for {} sectors: {}",
                 w.seq(), w.jset().sectors(fs.block_bits()), e.msg());
            j.to_text(&mut msg);
            fs.emergency_read_only(&mut msg);
        }
    }

    /* no_io: */
    w.cas = Default::default();

    w.continue_at(write_done, Some(&j.wq));
}

fn waitlist_splice(from: &c::journal_buf, to: &c::journal_buf) -> bool {
    let Some(waiters) = from.take_waiters(JOURNAL_BUF_NOFLUSH) else { return true };

    match to.add_waiters(waiters) {
        Ok(()) => true,
        Err(waiters) => {
            from.put_back_waiters(waiters, JOURNAL_BUF_NOFLUSH);
            false
        }
    }
}

fn flush_would_free_space(j: &c::journal, g: &JournalGuard<'_>, new_last_seq: u64) -> bool {
    let fs = j.fs();
    let rcu = read_lock();

    fs.rw_devs_for_rcu(DataType::journal, &rcu)
        .any(|ca| ca.journal_ring(g).would_free(new_last_seq))
}

impl c::journal {
    fn __should_flush(&self, g: &JournalGuard<'_>, w: &c::journal_buf, seq: u64) -> bool {
        /*
         * If the journal is in an error state - we did an emergency shutdown -
         * we prefer to continue doing journal writes. We just mark them as
         * noflush so they'll never be used, but they'll still be visible by the
         * list_journal tool - this helps in debugging.
         *
         * There's a caveat: the first journal write after marking the
         * superblock dirty must always be a flush write, because on startup
         * from a clean shutdown we didn't necessarily read the journal and the
         * new journal write might overwrite whatever was in the journal
         * previously - we can't leave the journal without any flush writes in
         * it.
         *
         * So if we're in an error state, and we're still starting up, we don't
         * write anything at all.
         */
        if self.error().is_err() {
            return false;
        }

        /* first write after filesystem was clean? */
        if self.flags.get(Journal::need_flush_write) {
            return true;
        }

        /* did we promise the allocator this write wouldn't be a commit? */
        if w.must_not_flush() {
            return false;
        }

        /* does journal reclaim need a flush? */
        if !self.flags.get(Journal::may_skip_flush) &&
            w.last_seq != self.lock.last_seq_ondisk.get() &&
            flush_would_free_space(self, g, w.last_seq) {
            return true;
        }

        let must_flush = w.must_flush();

        /*
         * With a flush outstanding, demote rather than queue a second flush
         * behind it: the waiters move to the next entry, whose flush - once the
         * outstanding one is done - takes this entry to disk with it. If there
         * isn't a next entry, we can't demote. Only entries with real flush
         * waiters can be demoted; a FLUSH_NO_WAIT transition entry has none
         * (flushers skip it) and must stay a flush write - and splicing it
         * would clobber the sentinel:
         */
        if buf_waiters(w.wait_word().load(Relaxed)) && g.flushes_outstanding != 0 {
            let next = if seq < self.fs().journal_cur_seq() { g.seq_to_buf(seq + 1) } else { None };

            if next.is_some_and(|next| waitlist_splice(w, next)) {
                /*
                 * Demoting to noflush: a later write will be the flush,
                 * and its preflush will sweep this entry's data to disk.
                 * Carry any flush request and its waiters forward to the
                 * next entry, which will ride that flush (cascading again
                 * if it too gets demoted, until it lands on the entry
                 * that becomes the flush). When that flush completes,
                 * flushed_seq_ondisk advances past all of them, so waking
                 * these waiters there is correct - no re-check needed.
                 *
                 * should_flush() never demotes the last in-flight entry
                 * while it has a flush request, so next != NULL here
                 * except on the emergency-read-only path - and there the
                 * waiters get an error from bch2_journal_error() anyway,
                 * so leaving them on this entry to wake at completion is
                 * fine.
                 */
                return false;
            }
        }

        if must_flush {
            return true;
        }

        time_after_eq(jiffies(),
                      g.last_flush_write.wrapping_add(msecs_to_jiffies(self.fs().opts().journal_flush_delay.get().into())))
    }

    fn should_flush(&self, g: &JournalGuard<'_>, w: &c::journal_buf, seq: u64) -> bool {
        self.__should_flush(g, w, seq) || !w.try_noflush()
    }

    pub fn do_writes_locked(&self, g: &mut JournalGuard<'_>) {
        let fs = self.fs();
        let seq = g.last_unallocated_seq();

        let flush = {
            let Some(w) = (seq != 0).then(|| g.seq_to_buf(seq)).flatten() else { return };

            if w.bits.write_started() ||
                self.state_seq_count(self.reservations(), seq) != 0 {
                return;
            }

            ebug_assert!(seq == w.seq(), "journal buf for seq {seq} holding {}", w.seq());

            let flush = self.should_flush(g, w, seq);

            if flush {
                /*
                 * A flush waits for the writes before it to complete - so for an
                 * outstanding flush to drain, too - and is decided only then, as
                 * it's started: the wait word is the decision, made once, and
                 * until it's made the write may yet become a noflush.
                 */
                if self.lock.seq_ondisk.get() + 1 != seq {
                    return;
                }

                /*
                 * Lost to bch2_journal_noflush_seq(): a noflush, unless it's the
                 * first write since the filesystem was clean, which must be a
                 * flush whatever was promised - as __should_flush() asks first:
                 */
                w.try_flush() || self.flags.get(Journal::need_flush_write)
            } else {
                false
            }
        };

        let rewind_limit = if flush {
            g.last_flush_write = jiffies();
            g.nr_flush_writes += 1;

            self.flags.set(Journal::need_flush_write, false);
            g.flushes_outstanding += 1;

            /*
             * (Re-)arm the auto-commit timer: if nothing else
             * commits first, close + write the open entry in at most
             * journal_flush_delay.
             */
            if seq != fs.journal_cur_seq() {
                self.write_work.modify(&self.wq, msecs_to_jiffies(fs.opts().journal_flush_delay.get().into()));
            } else {
                self.write_work.cancel();
            }

            if fs.opts().journal_rewind_discard_buffer_percent.get() == 0 {
                g.published().rewind_seq.set(seq + 1);
            }

            let mut r = c::jset_entry_rewind_limit::default();
            r.entry.type_ = c::JsetEntryType::rewind_limit.0 as u8;
            r.seq.set(self.lock.rewind_seq.get().min(seq + 1));
            Some(r)
        } else {
            g.nr_noflush_writes += 1;
            None
        };

        g.seq_write_started = seq;

        let w = g.seq_to_buf_mut(seq).expect("an in-flight journal buf");
        match rewind_limit {
            Some(r) => {
                w.append_entries(|ew| ew.push_entry(r).expect("rewind_limit_res's room"));
            }
            None => {
                w.head_mut().set_no_flush(true);
                w.head_mut().last_seq.set(0);
                w.last_seq = 0;
            }
        }
        w.bits.set_write_started(true);
        unsafe {
            c::closure_get(&raw const fs.cl as *mut _);
            crate::util::closure::call(NonNull::from(w), journal_write, Some(&self.wq));
        }
    }

    pub fn do_writes(&self) {
        self.do_writes_locked(&mut self.lock.lock());
    }
}
