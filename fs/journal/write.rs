// SPDX-License-Identifier: GPL-2.0

//! Journal writes: a journal buf's entry compacted, its devices allocated,
//! checksummed and submitted - a preflush first, if its flush goes
//! separately - and, once written, completed in seq order.

use core::pin::Pin;
use core::ptr::{self, NonNull};
use core::sync::atomic::Ordering::{Relaxed, SeqCst};

use cstruct_macros::in_place;
use pin_init::{pin_data, pin_init, stack_pin_init, PinInit};

use crate::alloc::disk_groups::Target;
use crate::util::alloc::flags;
use crate::alloc::replicas::DevlistEntry;
use crate::btree::bkey::prelude::*;
use crate::btree::bkey::{BkeySC, ValidateFlags};
use crate::btree::interior::btree_roots_to_journal_entries;
use crate::btree::write_buffer;
use crate::c;
use crate::c::JournalBucketStage::JOURNAL_BUCKET_cur;
use crate::c::JournalFlag as Journal;
use crate::c::JournalSpaceFrom::journal_space_discarded;
use crate::c::JournalGuard;
use crate::journal::flush_wait::{buf_waiters, JOURNAL_BUF_FLUSH, JOURNAL_BUF_FLUSH_NO_WAIT, JOURNAL_BUF_NOFLUSH,
                                 JOURNAL_BUF_NOT_IN_FLIGHT};
use crate::data::ptrs::{BkeyVPtrs, IoFailures, Ptrs};
use crate::data_structures::array_vec::ArrayVec;
use crate::debug::trace::events;
use crate::errcode::{Errcode, BchError};
use crate::format::DataType;
use crate::fs::{Dev, Fs};
use crate::journal::entries::{entry_u64s, EntryWriter};
use crate::journal::pins::PinsRead;
use crate::journal::validate::jset_validate;
use crate::data_structures::fifo::FifoRead;
use crate::locking::{Guard as _, LockedBy, MemallocFlags};
use crate::util::maybe_rcu_alloc::MaybeRcuAlloc;
use crate::sb::members::DevIoRef;
use crate::data::write::DevWriteRef;
use crate::os::workqueue::{Own, Queue};
use crate::util::alloc::{init_with, AllocError};
use crate::data::dev_bio::{DevBio, DevErrs};
use crate::data::write_replicas::{InFlight, Refs};
use crate::util::gather::Gather;
use crate::util::printbuf::Printbuf;
use crate::util::task::{requeue, waiter, Resume, Spawn, Task, TaskInPlace};
use crate::os::kernel::{jiffies, ktime_get_real_seconds, local_clock, msecs_to_jiffies, time_after_eq, unlikely,
                          WRITE};
use crate::util::rcu::read_lock;
use crate::util::refcount::TryRefcount;
use crate::util::vstructs::{cast_plain_slice_mut, SelfSized, ValidatedEntriesMut};
use crate::util::printbuf::ToText;
use crate::{log_msg_ratelimited, prt};

/* IOPRIO_PRIO_VALUE(IOPRIO_CLASS_RT, 0) */
const IOPRIO_RT: u16 = 1 << 13;

/*
 * The io refs write_alloc() took, in the order of the dev ptrs it appended to
 * the key: kept, so the alloc → submit (or alloc → no_io) gap doesn't have to
 * re-derive ca via c->devs[idx], which dev_remove may have cleared while our
 * io_ref still pins the dev object.
 */
type Cas = [Option<DevWriteRef>; c::BCH_REPLICAS_MAX as usize];

/// What close settled of an entry, read once as its write starts.
struct Closed {
    need_flush_to_write_buffer: bool,
    has_overwrites:             bool,
    sectors:                    u32,
    u64s_reserved:              u32,
}

/// What a write decided of its own as it allocated: its devices - the key's
/// pointers, an io ref on each - and whether its flush goes as a preflush of
/// its own. The key and separate_flush are published on the buf.
struct Allocated {
    key:            c::JournalKey,
    cas:            Cas,
    separate_flush: bool,
}

/// What a write wrote, to where, and what failed: the task's output, for its
/// done.
pub struct Written {
    /* the buf written, for its done */
    buf:          &'static c::journal_buf,
    /* the entry's buffer, the write's from write_start() on */
    data:         c::JsetBuf,
    devs:         c::bch_devs_list,
    failed:       c::bch_io_failures,
    /* preflushes go to every rw member with unflushed writes */
    flush_failed: IoFailures<{ c::BCH_SB_MEMBERS_MAX as usize }>,
    /* no keys, and its last_seq its own: nothing at or before it to replay */
    empty:        bool,
}

impl Written {
    fn new(buf: &'static c::journal_buf, data: c::JsetBuf) -> Self {
        Written {
            buf,
            data,
            devs:         Default::default(),
            failed:       Default::default(),
            flush_failed: Default::default(),
            empty:        false,
        }
    }

    /// What a gather of the write's bios failed on, noted - a preflush's in
    /// flush_failed - and those devices out of what's written.
    fn note_failed(&mut self, fs: &Fs, errs: impl IntoIterator<Item = (u8, c::blk_status_t)>, preflush: bool) {
        for (dev, status) in errs {
            if preflush {
                self.flush_failed.set_blk_status(fs, dev.into(), status);
            } else {
                self.failed.set_blk_status(fs, dev.into(), status);
            }
            self.devs.drop(dev);
        }
    }
}

impl c::JsetBuf {
    fn seq(&self) -> u64 {
        self.head().seq.get()
    }

    fn jset_and_tail(&mut self) -> (&mut [u8], &mut [u8]) {
        let bytes = self.jset().bytes();
        self.bytes_mut().split_at_mut(bytes)
    }

    /// Entries appended to its jset, by @f: the u64s they took, added to the
    /// header.
    fn append_entries(&mut self, f: impl FnOnce(&mut EntryWriter<'_>)) -> usize {
        let (_, tail) = self.jset_and_tail();
        let mut ew = EntryWriter::new(cast_plain_slice_mut(tail).expect("a journal buf, u64s past its jset"));
        f(&mut ew);
        let u64s = ew.len();

        let h = self.head_mut();
        h.set_u64s(h.u64s() + u64s);
        u64s
    }
}

impl c::journal_buf {
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

    /* A noflush made a flush: nothing waits on a noflush's word */
    fn flush_noflushed(&self) -> bool {
        self.wait_word().compare_exchange(JOURNAL_BUF_NOFLUSH, JOURNAL_BUF_FLUSH, SeqCst, Relaxed).is_ok()
    }

    /// The buffer start() moved into its write, out, for its task.
    fn write_take_buf(&self, g: &mut JournalGuard<'_>) -> c::JsetBuf {
        g.get_mut_through(&self.write, |w| &write_of(w).data).take()
            .expect("a started journal write's buffer, in the write")
    }

    /// Where its entries are, for the write buffer's flush: in the buf, or in
    /// its write once that's started, till its task takes them.
    pub(crate) fn wb_entries(&self, g: &mut JournalGuard<'_>) -> NonNull<[u8]> {
        if !g.get_of(&self.bits).write_started() {
            return self.data.entries_ptr(g);
        }
        let data = g.get_of(&write_of(g.get_of(&self.write)).data).as_ref()
            .expect("a started journal write's buffer, before its task takes it");
        data.entries_ptr(data.head().u64s())
    }
}

fn write_of(w: &Option<MaybeRcuAlloc<JournalWrite>>) -> &JournalWrite {
    w.as_deref().expect("a journal buf's write, made with its buffer")
}

impl c::journal {
    fn last_unallocated_seq(&self, g: &JournalGuard<'_>) -> u64 {
        self.in_flight.locked(g).iter_indexed()
            .find(|(_, buf)| !g.get_of(&buf.bits).write_allocated())
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

fn __write_alloc(j: &c::journal, a: &mut c::WriteAlloc, key: &mut c::JournalKey, seq: u64, cas: &mut Cas,
                 devs: &c::bch_devs_mask, sectors: u32, replicas: &mut u32, replicas_want: u32) {
    let fs = j.fs();

    /* the devices its key has a pointer to */
    let mut chosen = c::bch_devs_mask::default();
    if let Some(k) = BkeySC::from(&**key).arm::<Ptrs>() {
        for ptr in k.ptrs(fs) {
            chosen.d.set(ptr.dev().into());
        }
    }

    while *replicas < replicas_want {
        /*
         * Journal replicas spread across failure domains, like data:
         * one pick per sort, since each pick changes the occupancy
         * the next should avoid:
         */
        let mut devs_sorted = ::pin_init::zeroed::<c::dev_alloc_list>();
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
                chosen.d.test(cand.dev_idx.into()) ||
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
            ca.journal_ring_mut(&mut g).set_cur_seq(seq);
            bucket
        };

        let offset = ca.bucket_to_sector(bucket) + u64::from(bucket_size) - u64::from(ja.sectors_free.get());

        /*
         * Checked here, not at submit: flush writes are submitted after
         * their preflushes, so behind later noflush writes - a device's
         * submissions aren't in allocation order, and a ring lap can
         * make two consecutive ones the same sector, legitimately:
         */
        crate::warn_once!(offset == ca.prev_journal_sector.get(), fs, "journal write to {}:{offset} again", ca.dev_idx);
        ca.prev_journal_sector.set(offset);

        crate::data::extents::bkey_append_ptr(key, c::bch_extent_ptr::new().with_offset(offset).with_dev(ca.dev_idx))
            .expect("a journal key's pointers fit in key_pad");
        chosen.d.set(ca.dev_idx.into());

        /* Stash ca alongside the just-appended ptr; submit + no_io
         * walk @key.k.u64s ptrs in order, so the index lines up. */
        let idx = key.k.val_u64s() - 1;

        ja.sectors_free.set(ja.sectors_free.get() - sectors);
        *replicas += u32::from(ca.mi.get().durability);
        cas[idx] = Some(ca);
    }
}

fn write_alloc(j: &c::journal, a: &mut c::WriteAlloc, key: &mut c::JournalKey, data: &c::JsetBuf, cas: &mut Cas,
               replicas: &mut u32) -> Result<(), BchError>
{
    let fs = j.fs();
    let sectors = data.jset().sectors(fs.block_bits()) as u32;
    let mut target = match fs.opts().metadata_target.get() {
        0 => fs.opts().foreground_target.get(),
        t => t,
    };
    let replicas_want = fs.journal_replicas();
    let seq = data.seq();

    'target: loop {
        let devs = fs.target_rw_devs(DataType::journal, Target::decode(target.into()));

        /* The last write's devices, if out of room, move to their next bucket: */
        let mut prev = devs;
        prev.d.and(&a.wp_prev_devs.d);
        advance_devs_to_next_bucket(j, &prev, sectors, seq);

        __write_alloc(j, a, key, seq, cas, &devs, sectors, replicas, replicas_want);
        if *replicas >= replicas_want {
            break;
        }

        advance_devs_to_next_bucket(j, &devs, sectors, seq);
        __write_alloc(j, a, key, seq, cas, &devs, sectors, replicas, replicas_want);
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

    /* The devices it's allocated, a pointer each: bkey_val_u64s(&w->key.k) */
    let nr_ptrs = key.k.val_u64s();
    assert!(nr_ptrs <= c::BCH_REPLICAS_MAX as usize, "journal write to {nr_ptrs} devices");

    if *replicas != 0 {
        a.wp_prev_devs = c::bch_devs_mask::default();
        if let Some(k) = BkeySC::from(&**key).arm::<Ptrs>() {
            for ptr in k.ptrs(fs) {
                a.wp_prev_devs.d.set(ptr.dev().into());
            }
        }
    }

    if *replicas != 0 {
        Ok(())
    } else {
        Err(fs.err(Errcode::insufficient_journal_devices))
    }
}

fn buf_realloc(j: &c::journal, data: &mut c::JsetBuf) {
    let fs = j.fs();

    /* we aren't holding j->lock: */
    let new_size = j.lock.buf_size_want.get();

    if data.size() >= new_size {
        return;
    }

    let btree_write_buffer_size = new_size as usize / 64;

    if fs.btree_write_buffer_resize(btree_write_buffer_size).is_err() {
        return;
    }

    /*
     * Speculative growth, and every failure path here just declines to
     * grow - but we're under j->buf_lock, which the next journal write and
     * the btree write buffer's flush need, so sitting in reclaim for a bigger
     * buffer stalls them. Take the failure immediately instead.
     */
    let Ok(mut new_buf) = c::JsetBuf::new(new_size, flags::NOIO | flags::NORETRY | flags::NOWARN) else {
        return;
    };

    new_buf.bytes_mut()[..data.size() as usize].copy_from_slice(data.bytes());

    *data = new_buf;
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
                None => refs.try_push((devs_r, 1)).map_err(|_| j.fs().err(Errcode::ENOMEM_journal_buf))?,
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
fn last_uncompleted_write_seq(j: &c::journal, g: &JournalGuard<'_>, seq_completing: u64) -> u64 {
    let in_flight = j.in_flight.locked(g);
    let Some(front) = in_flight.peek_front() else { return 0 };
    let seq = in_flight.front_idx();

    if g.get_of(&front.bits).write_done() || seq == seq_completing { seq } else { 0 }
}

fn write_done(w: &c::journal_buf, written: Written) {
    let j = w.journal();
    let fs = j.fs();
    let mut err = Ok(());

    /*
     * kvfree can sleep, so it can't run under j->lock (or any lock here):
     * bufs_to_free is dropped after every lock is.
     */
    let bufs_to_free;

    let data = written.data;
    let seq_wrote = data.seq();

    let no_flush = data.head().no_flush();
    let time = if no_flush {
        c::TimeStat::journal_noflush_write
    } else {
        c::TimeStat::journal_flush_write
    };
    fs.times.get(time as usize).update(j.write_start_time.get());

    /*
     * pin_resize_lock held across the journal pin FIFO updates below; it
     * keeps the pin_list stable while we rebuild temp replicas entries from
     * the compact device list for refcount updates.
     */
    let mut pins = j.pins();
    let write_failed = !written.failed.is_empty();
    let flush_failed = !written.flush_failed.is_empty();

    if unlikely(write_failed || flush_failed) {
        fs.replicas_entry_put_many(&DevlistEntry::new(DataType::journal, pins.seq_devs(seq_wrote)), 1);
        pins.seq_set_devs(seq_wrote, &c::bch_devs_list::default());
    }

    if pins.seq_devs(seq_wrote).nr == 0 && !written.empty {
        err = fs.replicas_entry_get(&DevlistEntry::new(DataType::journal, written.devs));
        if err.is_ok() {
            pins.seq_set_devs(seq_wrote, &written.devs);
        }
    }

    if unlikely(write_failed || flush_failed || err.is_err()) {
        if written.devs.nr == 0 {
            err = Err(fs.err(Errcode::journal_write_err));
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
        let mut msg = if err.is_ok() && written.failed.all_dev_removed() && written.flush_failed.all_dev_removed() {
            let mut msg = crate::util::log::LogMsg::new(fs);
            msg.set_suppressed(true);
            msg
        } else {
            /* Separate ratelimit_states for hard and soft errors */
            log_msg_ratelimited!(fs, err.is_err() as usize, 2)
        };

        prt!(&mut *msg, "error writing journal entry {seq_wrote}\n");
        written.failed.to_text(&mut msg, fs);
        if flush_failed {
            prt!(&mut *msg, "preflush errors:\n");
            written.flush_failed.to_text(&mut msg, fs);
        }

        match err {
            Ok(()) => {
                let key = *j.lock.lock().get_of(&w.key);
                if written.devs.nr < BkeySC::from(&*key).arm::<Ptrs>().map_or(0, |k| k.devs(fs).nr) {
                    prt!(&mut *msg, "wrote degraded to ");
                    fs.devs_list_to_text(&mut msg, &written.devs);
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
    g.get_mut_of(&w.bits).set_empty(written.empty);

    /* the buffer and write, together: the next entry's, if bigger than what's there */
    let write = g.get_mut_of(&w.write).take().expect("a journal write's own, in its buf");
    bufs_to_free = if g.free_buf.as_ref().map_or(0, |b| b.size()) < data.size() {
        (g.free_buf.replace(data), g.free_write.replace(write))
    } else {
        (Some(data), Some(write))
    };
    let in_flight = g.get_mut_of(&w.in_flight).take();

    let (mut g, done) = complete_writes(j, g, &pins, seq_wrote);

    /*
     * Mark our buffer's bookkeeping done. Two cases:
     *
     *  - complete_writes() popped past us (front advanced beyond seq_wrote):
     *    in_flight.get() returns None, nothing to do - the pop already
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
    if let Some(w) = j.in_flight.locked(&g).get(seq_wrote) {
        g.get_mut_of(&w.bits).set_write_done(true);
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

        fs.times.get(c::TimeStat::blocked_journal_max_in_flight as usize).track_event_change(false);

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
    drop(bufs_to_free);

    if done.last_seq_ondisk_moved {
        fs.reset_alloc_cursors();
        fs.do_discards_async();
    }

    drop(in_flight);
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
        let seq = last_uncompleted_write_seq(j, &g, seq_wrote);
        if seq == 0 {
            break;
        }

        let w = j.in_flight.locked(&g).get(seq).expect("an in-flight journal buf");
        let (last_seq, empty, must_not_flush) = (*g.get_of(&w.last_seq), g.get_of(&w.bits).empty(), w.must_not_flush());

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
        /* in_flight's front, past the buf: its data write_done() already took */
        g.published().seq_ondisk.set(seq);

        if let Some(waiters) = w.take_waiters(JOURNAL_BUF_NOT_IN_FLIGHT) {
            waiters.wake();
        }

        done.any = true;
    }

    (g, done)
}

/// In the last bio's endio, of a flush.
fn wake_flush_waiters(w: &c::journal_buf, written: &Written) {
    /*
     * Wake up flush waiters early, if there wasn't an error:
     *
     * Flush writes wait for previous outstanding writes, so there's no
     * ordering concerns here (as journal_write_done normally has to
     * handle). Don't mark it as closed to new flush waiters yet, since
     * we're not taking j->lock and updating the various seq_ondisk fields
     * yet:
     */
    if written.failed.is_empty() &&
        written.flush_failed.is_empty() &&
        buf_waiters(w.wait_word().load(Relaxed)) {
        if let Some(waiters) = w.take_waiters(JOURNAL_BUF_FLUSH) {
            waiters.wake();
        }
    }
}

type PreflushErrs = DevErrs<{ c::BCH_SB_MEMBERS_MAX as usize }>;

/// The entry's write: its op, FUA for a flush and PREFLUSH if there's no
/// separate one, traced, and its bytes.
fn write_opf(fs: &Fs, data: &c::JsetBuf, key: &c::JournalKey, separate_flush: bool) -> (u32, u32) {
    let sectors = data.jset().sectors(fs.block_bits()) as u32;
    let flush = !data.head().no_flush();

    fs.event_inc_trace(events::journal_write, |out| {
        prt!(out, "seq {} flush {} sectors {sectors}\n", data.seq(), flush as u32);
        prt!(out, "{}", BkeySC::from(&**key).to_text(fs));
    });

    /*
     * blk-wbt.c throttles all writes except those that have both
     * REQ_SYNC and REQ_IDLE set...
     */
    let mut opf = c::req_op_REQ_OP_WRITE | c::REQ_SYNC | c::REQ_IDLE | c::REQ_META;
    if flush {
        opf |= c::REQ_FUA;
    }
    if flush && !separate_flush {
        opf |= c::REQ_PREFLUSH;
    }
    (opf, sectors << 9)
}

/// An empty REQ_PREFLUSH to every rw member with unflushed writes, or in
/// @devs, which the entry's going to, submitted to @g.
fn write_preflush(w: &c::journal_buf, devs: &c::bch_devs_list, g: &Gather<PreflushErrs, waiter::Async>) {
    let fs = w.journal().fs();
    let _noio = MemallocFlags::noio();

    for ca in fs.rw_members() {
        /*
         * A member with no completed writes since its last
         * flush has nothing for this flush to make durable:
         */
        if !ca.flags.swap(c::DevFlag::unflushed_writes, false) &&
            !devs.has(ca.dev_idx) {
            continue;
        }

        let opf = c::req_op_REQ_OP_WRITE | c::REQ_SYNC | c::REQ_IDLE | c::REQ_META | c::REQ_PREFLUSH;
        let bio = ca.journal.bio_set.alloc::<BchError, _>(0, opf, |b| DevBio::new(b.init()))
            .expect("a journal preflush: its bioset waits for one");
        bio.submit_gathered(ca.into(), 0, g.get());
    }
}

/// The entries write_prep() adds to every journal entry besides its btree
/// roots - the datetime, and super_entries_add_common()'s: what
/// super_entries_journal_res reserves.
pub(crate) const PREP_ENTRIES_U64S: usize =
    entry_u64s::<c::jset_entry_datetime>() + crate::sb::clean::COMMON_U64S;

/// The entry compacted, and whether it's empty.
fn write_prep(j: &c::journal, w: &c::journal_buf, data: &mut c::JsetBuf, closed: &Closed) -> Result<bool, BchError> {
    let fs = j.fs();
    let mut wb = None;
    let mut btree_roots_have = 0u64;
    let seq = data.seq();

    /*
     * The write's flush or noflush, as do_writes_locked() left its wait word -
     * final by now - into the header: a flush's rewind limit, or a noflush's
     * flag and no last_seq, which empty below is reckoned on.
     */
    if w.must_not_flush() {
        let h = data.head_mut();
        h.set_no_flush(true);
        h.last_seq.set(0);
    } else {
        let mut r = c::jset_entry_rewind_limit::default();
        r.seq.set(j.lock.rewind_seq.get().min(seq + 1));
        data.append_entries(|ew| ew.push_entry(0, 0, r).expect("rewind_limit_res's room"));
    }

    let mut empty = data.head().seq.get() == data.head().last_seq.get();

    if closed.need_flush_to_write_buffer {
        wb = Some(write_buffer::Intake::start(fs, seq));

        /*
         * need_flush_to_write_buffer must be cleared under the write buffer's
         * locks, dropped by bch2_journal_keys_to_write_buffer_end(), to avoid
         * racing with write buffer flushing
         */
        j.lock.lock().get_mut_of(&w.bits).set_need_flush_to_write_buffer(false);
    }

    /*
     * Simple compaction, dropping empty jset_entries (from journal
     * reservations that weren't fully used) and merging jset_entries that
     * can be.
     *
     * If we wanted to be really fancy here, we could sort all the keys in
     * the jset and drop keys that were overwritten - probably not worth it:
     */
    // closed: every reservation's entries are in it, whole - not encrypted
    // till write_checksum()
    let jset = unsafe { data.jset_mut().assume_validated_mut() };
    for i in ValidatedEntriesMut::of(jset) {
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

    let u64s = data.append_entries(|ew| {
        btree_roots_to_journal_entries(fs, ew, btree_roots_have).expect("root_journal_res's room");

        let mut d = c::jset_entry_datetime::default();
        d.seconds.set(ktime_get_real_seconds() as u64);
        ew.push_entry(0, 0, d).expect("super_entries_journal_res's room");

        crate::sb::clean::super_entries_add_common(fs, ew).expect("super_entries_journal_res's room");
    });

    crate::warn_on!(u64s > j.lock.entry_u64s_reserved.get() as usize);

    let sectors = data.jset().sectors(fs.block_bits()) as u32;

    if sectors > closed.sectors {
        crate::fs_fatal_error!(fs, ": journal write overran available space, {} > {} (extra {u64s} reserved {}/{})",
                               data.jset().bytes(), closed.sectors << 9, closed.u64s_reserved,
                               j.lock.entry_u64s_reserved.get());
        return Err(fs.err(Errcode::EINVAL_journal_write_overran_available_space));
    }

    Ok(empty)
}

fn write_checksum(j: &c::journal, data: &mut c::JsetBuf, closed: &Closed) -> Result<(), BchError> {
    let fs = j.fs();
    let csum_type = fs.meta_checksum_type();
    let mut validate_before_checksum = false;

    let h = data.head_mut();

    h.magic.set(fs.jset_magic());
    h.version.set(fs.sb.version.get());

    h.set_big_endian(cfg!(target_endian = "big"));
    h.set_csum_type(csum_type);
    h.set_has_overwrites(closed.has_overwrites);

    if csum_type.is_encryption() {
        validate_before_checksum = true;
    }

    if data.head().version.get() < c::MetadataVersion::CURRENT {
        validate_before_checksum = true;
    }

    if validate_before_checksum {
        jset_validate(fs, None, data.jset_and_tail().0, 0, ValidateFlags::write)?;
    }

    if let Err(e) = data.jset_mut().encrypt(fs) {
        crate::fs_fatal_error!(fs, "encrypting journal entry: {}", e.msg());
        return Err(e);
    }

    data.head_mut().csum = data.jset().checksum(fs);

    if !validate_before_checksum {
        jset_validate(fs, None, data.jset_and_tail().0, 0, ValidateFlags::write)?;
    }

    let sectors = data.jset().sectors(fs.block_bits());
    let bytes = data.jset().bytes();
    data.bytes_mut()[bytes..sectors << 9].fill(0);
    Ok(())
}

/// The entry's buffer, out of its write for its task, and what close
/// settled of it - under buf_lock, held till write_prep() is done: the write
/// buffer's flush reads the buffer in the write till then.
fn write_take(w: &'static c::journal_buf, seq: u64, _: &c::BufLockGuard<'_>) -> (Closed, Written) {
    let mut l = w.journal().lock.lock();
    let bits = l.get_of(&w.bits);
    assert!(bits.write_started(), "journal write of seq {seq} not started");
    assert!(!bits.write_allocated(), "journal write of seq {seq} already allocated");
    assert!(!bits.write_done(), "journal write of seq {seq} already done");

    let closed = Closed {
        need_flush_to_write_buffer: bits.need_flush_to_write_buffer(),
        has_overwrites:             bits.has_overwrites(),
        sectors:                    *l.get_of(&w.sectors),
        u64s_reserved:              *l.get_of(&w.u64s_reserved),
    };

    let data = w.write_take_buf(&mut l);
    (closed, Written::new(w, data))
}

/// The entry compacted - under @b, its buf_lock, from write_take() - its
/// devices allocated - returned - and checksummed, its replicas marked.
fn write_start(w: &c::journal_buf, seq: u64, b: c::BufLockGuard<'_>, closed: &Closed, written: &mut Written)
    -> Result<Allocated, BchError>
{
    let j = w.journal();
    let fs = j.fs();
    let nr_rw_members = fs.allocator.rw_devs[DataType::free].weight();

    let _noio = MemallocFlags::noio();

    let data = &mut written.data;
    ebug_assert!(seq == data.seq(), "journal buf for seq {seq} holding {}", data.seq());
    buf_realloc(j, data);
    written.empty = write_prep(j, w, data, closed)?;
    drop(b);

    let ret = j.error();
    if ret.is_err() && j.flags.get(Journal::need_flush_write) {
        ret?;
    }

    let mut key = c::JournalKey::empty();
    let mut cas = Cas::default();
    let mut replicas_allocated = 0;
    let ret = loop {
        let ret = write_alloc(j, &mut j.write_alloc.lock(), &mut key, data, &mut cas, &mut replicas_allocated);
        if ret.is_ok() || !j.lock.can_discard.get() {
            break ret;
        }

        j.do_discards();
    };
    ret?;

    write_checksum(j, data, closed)?;

    let separate_flush = nr_rw_members > 1;
    written.devs = BkeySC::from(&*key).arm::<Ptrs>().map(|k| k.devs(fs)).unwrap_or_default();

    {
        let mut g = j.lock.lock();
        assert!(j.last_unallocated_seq(&g) == seq,
                "journal writing seq {seq}, but the first unallocated is {}", j.last_unallocated_seq(&g));

        /*
         * write is allocated, no longer need to account for it in
         * bch2_journal_space_available():
         */
        *g.get_mut_of(&w.sectors) = 0;
        g.get_mut_of(&w.bits).set_write_allocated(true);
        g.entry_bytes_written += data.jset().bytes() as u64;

        g.get_mut_of(&w.bits).set_separate_flush(separate_flush);
        *g.get_mut_of(&w.key) = key;

        /*
         * journal entry has been compacted and allocated, recalculate space
         * available:
         */
        j.space_available(&mut g);
        j.do_writes_locked(&mut g);
    }

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

        if let Err(e) = fs.replicas_entry_get(&DevlistEntry::new(DataType::journal, written.devs)) {
            pins.seq_set_devs(seq, &c::bch_devs_list::default());
            return Err(e);
        }

        pins.seq_set_devs(seq, &written.devs);
    }

    Ok(Allocated { key, cas, separate_flush })
}

/// write_start()'s error: the filesystem goes emergency read-only, and the
/// write does no IO.
fn write_start_err(w: &c::journal_buf, data: &c::JsetBuf, e: BchError) {
    let j = w.journal();
    let fs = j.fs();
    let mut msg = crate::util::log::LogMsg::new(fs);
    msg.set_suppressed(true); /* only print once, when we go ERO */

    prt!(&mut *msg, "Unable to do journal write at seq {} for {} sectors: {}",
         data.seq(), data.jset().sectors(fs.block_bits()), e.msg());
    j.to_text(&mut msg);
    fs.emergency_read_only(&mut msg);
}

/// A journal entry written: it, where to and what failed, for its done.
#[in_place(WriteFut)]
async fn write_task(w: &'static c::journal_buf, seq: u64) -> Written {
    let j = w.journal();
    let fs: &'static Fs = j.fs();
    let wq: &'static Queue = &j.wq;

    j.write_start_time.set(local_clock());

    /*
     * The entry's buffer, ours - but the write buffer reads its body
     * out of the write till write_prep() clears need_flush_to_write_buffer:
     * buf_lock held till then.
     */
    let (mut written, a) = {
        let b = j.buf_lock.lock();
        let (closed, mut written) = write_take(w, seq, &b);
        let a = write_start(w, seq, b, &closed, &mut written);
        (written, a)
    };

    let a = match a {
        Ok(a) => a,
        Err(e) => {
            write_start_err(w, &written.data, e);
            return written;
        }
    };
    if fs.opts().nochanges.get() != 0 {
        return written;
    }

    let flush = !written.data.jset().no_flush();

    if flush && a.separate_flush {
        stack_pin_init!(let g = Gather::<PreflushErrs, waiter::Async>::new());
        write_preflush(w, &written.devs, &g);
        let errs = g.wait(Resume::queue(wq)).await;
        written.note_failed(fs, errs.0, true);
    }

    let Allocated { key, mut cas, separate_flush } = a;
    let data = &written.data;
    let (opf, bytes) = write_opf(fs, data, &key, separate_flush);
    // the replicas are clones of the buffer's bio, from the journal's own set:
    // they don't wait behind data writes'
    let mut io = InFlight::with(fs, |ca| &ca.journal.bio_set, IOPRIO_RT);
    io.submit::<c::DevWriteRefKind>(data.bio_range(bytes), opf, DataType::journal, &key, Refs::Given(&mut cas),
                                    Resume::queue(wq)).await;
    let errs = io.wait(Resume::INLINE).await;
    written.note_failed(fs, errs.failed(), false);
    if flush {
        wake_flush_waiters(w, &written);
    }
    requeue(wq).await;

    written
}

/// What writes a journal entry, made with its buffer: the task, made for each
/// write where the last one finished.
#[pin_data]
pub struct JournalWrite {
    #[pin]
    task: TaskInPlace<'static, WriteFut>,
    /*
     * The buffer start() moves in, till the task takes it: the write buffer's
     * flush reads it here till then.
     */
    data: c::BufLocked<Option<c::JsetBuf>>,
}

impl JournalWrite {
    pub fn new() -> impl PinInit<Self, AllocError> {
        pin_init!(Self {
            task <- TaskInPlace::new(),
            data: LockedBy::new(None, ()),
        }? AllocError)
    }

    /// The task that writes @buf's entry @seq - its write started under
    /// j->lock, @buf holding this write - made where the last write's was, and
    /// started on the journal's workqueue.
    fn start(self: Pin<&mut Self>, buf: &'static c::journal_buf, seq: u64, data: c::JsetBuf) {
        let j = buf.journal();
        let w = self.project();
        assert!(w.data.get_mut().replace(data).is_none(), "a journal write started, holding the last one's buffer");

        w.task.start(Spawn::Queue(&j.wq), j.fs().tasks(), init_with(move || write_task(buf, seq)), write_task_done)
            .expect("a task on a workqueue: only a thread's fails");
    }

    pub fn to_text(&self, out: &mut Printbuf) {
        match self.task.get() {
            Some(task) => task.task().to_text(out),
            None       => prt!(out, "never started\n"),
        }
    }
}

/// The write's done, where its task finished: on the journal's workqueue.
fn write_task_done(_: NonNull<Task<'static, WriteFut>>, written: Written) {
    write_done(written.buf, written);
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
        let last_seq = *g.get_of(&w.last_seq);
        if !self.flags.get(Journal::may_skip_flush) &&
            last_seq != self.lock.last_seq_ondisk.get() &&
            flush_would_free_space(self, g, last_seq) {
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
            let next = if seq < self.fs().journal_cur_seq() { self.in_flight.locked(g).get(seq + 1) } else { None };

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
        /* ourself, 'static: the write's task has its buf */
        let j = self.in_flight.journal();
        let seq = self.last_unallocated_seq(g);
        let Some(w) = (seq != 0).then(|| j.in_flight.locked(g).get(seq)).flatten() else { return };

        let flush = {
            if g.get_of(&w.bits).write_started() ||
                self.state_seq_count(self.reservations(), seq) != 0 {
                return;
            }

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
                 * flush whatever was promised - as __should_flush() asks first.
                 * The word's made a flush then too: it's what completion and
                 * noflush_seq() go by, and write_prep() writes the header from.
                 */
                w.try_flush() || (self.flags.get(Journal::need_flush_write) && w.flush_noflushed())
            } else {
                false
            }
        };

        if flush {
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
                self.write_work.modify(Own, msecs_to_jiffies(fs.opts().journal_flush_delay.get().into()));
            } else {
                self.write_work.cancel();
            }

            if fs.opts().journal_rewind_discard_buffer_percent.get() == 0 {
                g.published().rewind_seq.set(seq + 1);
            }
        } else {
            g.nr_noflush_writes += 1;
            *g.get_mut_of(&w.last_seq) = 0;
        }

        g.seq_write_started = seq;

        g.get_mut_of(&w.bits).set_write_started(true);
        *g.get_mut_of(&w.in_flight) =
            Some(self.writes_in_flight.tryget().expect("a journal write after the filesystem's exit"));

        let data = w.data.take(g);
        let write = g.get_mut_of(&w.write).as_mut().expect("a journal buf's write, made with its buffer");
        write.as_pin_mut().start(w, seq, data);
    }

    pub fn do_writes(&self) {
        self.do_writes_locked(&mut self.lock.lock());
    }
}
