// SPDX-License-Identifier: GPL-2.0

//! Discards and invalidates - discard.c's.
//!
//! A bucket emptied goes need_discard, keyed by the journal seq that emptied
//! it. It's free again only once its old contents can't be read back: that
//! journal entry flushed, and the journal's rewind point past it. The discard
//! worker walks need_discard, oldest first, up to what the journal allows,
//! and for each bucket issues a discard - committing it free when the discard
//! completes - or, on a device that doesn't take them, commits it free at
//! once: Worker::mark_free_key(), the one writer of a free bucket.
//!
//! A worker's discards are its own: each pass of the discard worker, and each
//! run of a device's fast discard work, has a Gather - Discards - its bios put
//! into as they complete, each its Discard: the bucket, and its device's
//! write io ref. The worker reaps them as it goes, marking their buckets
//! free, and waits out the rest at the end. The io ref is held until the
//! bucket's marked free, so device removal can't delete the alloc range under
//! an update still to commit.
//!
//! Shared between the workers: each device's count of discards in flight, at
//! most DEV_IN_FLIGHT_MAX - one more for the fast path - so they don't
//! saturate it, a worker refused sleeping on the count till one completes;
//! and c->discards' in_flight, the buckets with a discard issued and not yet
//! marked free, so no bucket has two: the second could land after the first
//! had it marked free, and the bucket was written again.
//!
//! Fast discards: a bucket whose emptying needed no journal flush - the alloc
//! trigger's call - skips the wait in need_discard, queued on its device's
//! discard_fast for the device's fast discard work.
//!
//! Free space short, the worker releases buckets for discard by advancing the
//! journal's rewind seq, flushing the journal, and flushing the write buffer,
//! where need_discard updates from the alloc trigger may be:
//! calculate_discard_sectors_to_release().
//!
//! Invalidates: a device short of free buckets turns over cached ones, its
//! read lru oldest first, dropping its pointers from what each bucket's
//! backpointers point to.
//!
//! The work items are ReffedWorks: the worker holds a write ref, a device's
//! fast discard and invalidate works the device's write io ref and a write
//! ref - so going read-only, and a device going offline, wait for them.

use crate::accounting::data_type;
use crate::alloc::background::alloc_to_v4;
use crate::alloc::backpointers::{backpointer_get_key, BackpointerPos};
use crate::alloc::buckets::{bucket_to_u64, u64_to_bucket};
use crate::alloc::capacity::DiskReservation;
use crate::alloc::check_backpointers::check_one_bucket_backpointer_mismatch;
use crate::alloc::lru::LruPos;
use crate::btree::bkey::prelude::*;
use crate::btree::bkey::{pos, BkeySC, POS_MIN};
use crate::btree::bkey_buf::BkeyBuf;
use crate::btree::iter::{is_restart, lockrestart_do, BtreeIter, BtreeIterFlags, BtreeTrans, CommitFlags, CommitOpts,
                         LoopFlow, TransAttempt, TransBkey, UpdateTriggerFlags, Watermark};
use crate::btree::write_buffer::{WbFlush, WbMaybeFlush};
use crate::c;
use crate::c::bch_errcode;
use crate::data::ptrs::Ptrs;
use crate::errcode::BchError;
use crate::fs::Fs;
use crate::init::damage;
use crate::init::error::id;
use crate::locking::{MemallocFlags, SeqLock};
use crate::sb::counters::events;
use crate::sb::members::DevIoRef;
use crate::util::alloc::{flags::GFP_KERNEL, KVVec};
use crate::util::bio::{Bio, BioBox, BioOwner, Endio, RawBio};
use crate::util::ffi::Opaque;
use crate::util::futex::{futex_wait, futex_wake};
use crate::util::gather::{Gather, GatherRef};
use crate::util::kernel::{bdev_max_discard_sectors, local_clock, READ};
use crate::util::place::Placed;
use crate::util::log::LogMsg;
use crate::util::rcu::read_lock;
use crate::util::sleep::Sleep;
use crate::util::{HumanReadable, Printbuf};
use crate::{bch_err_fn, bch_err_msg, fsck_err, has_bio, prt};
use core::sync::atomic::Ordering;
use cstruct_macros::c_export;
use pin_init::{pin_data, pin_init, stack_pin_init};

/// Discards in flight at once on a device.
const DEV_IN_FLIGHT_MAX: u8 = 4;

// ── State ────────────────────────────────────────────────────────────────

/// What became of the buckets a discard worker looked at, in sectors: each
/// bucket counted once, by why it was passed over or what was done.
#[derive(Clone, Copy, Default)]
pub(crate) struct DiscardState {
    seen:                u64,
    not_rw:              u64,
    eexist:              u64,
    eagain:              u64,
    open:                u64,
    need_journal_commit: u64,
    need_rewind_advance: u64,
    bad_data_type:       u64,
    discarded:           u64,
    committed:           u64,
    r:                   DiscardRelease,
}

/// What the worker releases for discard when free space is short, in
/// sectors: calculate_discard_sectors_to_release()'s.
#[derive(Clone, Copy, Default)]
pub(crate) struct DiscardRelease {
    buffer:         u64,
    pending_total:  u64,
    free:           u64,
    reserve:        u64,
    buffer_clamped: u64,
    release:        i64,
    new_rewind_seq: u64,
    flush_journal:  bool,
    flush_wb:       bool,
}

/// A discard issued, until its bucket's marked free: its claim on the bucket
/// in c->discards.in_flight, released when it's dropped, and its device's
/// write io ref.
pub(crate) struct Discard {
    ca:         DevIoRef<c::bch_dev_write_ref>,
    dev_bucket: u64,
}

/// A worker's discards: those in flight, and those done, to reap.
type Discards = Gather<KVVec<Discard>>;

enum NotAdded {
    /// One is in flight for the bucket already.
    Exists,
    /// The device has as many in flight as it may - or there was no memory.
    Full(BchError),
}

/// A run of discards - a pass of the discard worker, or a run of a device's
/// fast discard work: its Discards, room for those it reaps, and its counts.
struct Worker<'a> {
    g:        &'a Discards,
    done:     KVVec<Discard>,
    s:        &'a SeqLock<DiscardState>,
    fastpath: bool,
}

/// A discard bio: its Discard, and a ref on its worker's Discards to put it
/// with - Options, for the endio to take.
#[pin_data]
#[repr(C)]
pub(crate) struct DiscardBio {
    done:    Option<GatherRef<KVVec<Discard>>>,
    discard: Option<Discard>,
    #[pin]
    bio:     Bio,
}
has_bio!(DiscardBio, bio);

// ── to_text ──────────────────────────────────────────────────────────────

fn discard_state_to_text(out: &mut Printbuf, s: &DiscardState) {
    out.tabstop_push(32);
    prt!(out, "seen:\t{}\n", s.seen);
    prt!(out, "not_rw:\t{}\n", s.not_rw);
    prt!(out, "eexist:\t{}\n", s.eexist);
    prt!(out, "eagain:\t{}\n", s.eagain);
    prt!(out, "open:\t{}\n", s.open);
    prt!(out, "need_journal_commit:\t{}\n", s.need_journal_commit);
    prt!(out, "need_rewind_advance:\t{}\n", s.need_rewind_advance);
    prt!(out, "bad_data_type:\t{}\n", s.bad_data_type);
    prt!(out, "discarded:\t{}\n", s.discarded);
    prt!(out, "committed:\t{}\n", s.committed);
}

/// Sectors, as bytes - the release calculation's figures are all sectors.
fn prt_sectors(out: &mut Printbuf, label: &str, sectors: i64) {
    prt!(out, "{label}:\t{}\n", HumanReadable(sectors << 9));
}

impl Fs {
    pub fn discards_to_text(&self, out: &mut Printbuf) {
        let d = &self.discards;
        let s = d.s.read();
        discard_state_to_text(out, &s);

        prt!(out, "Discard release:\n");
        {
            let mut out = out.indent(2);
            prt_sectors(&mut out, "buffer", s.r.buffer as i64);
            prt_sectors(&mut out, "pending_total", s.r.pending_total as i64);
            prt_sectors(&mut out, "free", s.r.free as i64);
            prt_sectors(&mut out, "reserve", s.r.reserve as i64);
            prt_sectors(&mut out, "buffer_clamped", s.r.buffer_clamped as i64);
            prt_sectors(&mut out, "release", s.r.release);
            prt!(out, "flush_journal:\t{}\n", u8::from(s.r.flush_journal));
            prt!(out, "flush_wb:\t{}\n", u8::from(s.r.flush_wb));
        }

        let j = &self.journal;
        prt!(out, "journal seq:\t{}\n", self.journal_cur_seq());
        prt!(out, "journal flushed seq:\t{} -> {}\n",
             j.flushing_seq.load(core::sync::atomic::Ordering::Relaxed), j.flushed_seq_ondisk.get());
        prt!(out, "journal rewind seq:\t{} -> {}\n", j.rewind_seq.get(), j.rewind_seq_ondisk.get());

        prt!(out, "In flight:\n");
        let mut out = out.indent(2);
        let mut out = out.atomic();
        // a mutex: taken before RCU, never under it
        let in_flight = d.in_flight.lock();
        let rcu = read_lock();
        for &b in in_flight.iter() {
            let b = u64_to_bucket(b);
            let name = self.dev_rcu(b.inode as u32, &rcu).map_or(c"?", |ca| ca.name());
            prt!(out, "{}:{}\n", name.to_str().unwrap_or("?"), { b.offset });
        }
    }
}

impl c::bch_dev {
    pub fn fast_discards_to_text(&self, out: &mut Printbuf) {
        prt!(out, "{}\n", self.discard_fast.lock().len());
    }
}

// ── Discards in flight ───────────────────────────────────────────────────

fn dev_bucket_size(fs: &Fs, dev: u64) -> u64 {
    let rcu = read_lock();
    fs.dev_rcu(dev as u32, &rcu).map_or(0, |ca| ca.mi.get().bucket_size.into())
}

/// Whether @bucket is one the allocator mustn't use - a device that isn't
/// there, as one.
fn dev_bucket_nouse(fs: &Fs, bucket: c::bpos) -> bool {
    let rcu = read_lock();
    fs.dev_rcu(bucket.inode as u32, &rcu).is_none_or(|ca| ca.bucket_nouse(bucket.offset))
}

/// Discards on, for @ca: the mount option, if it was given, or the device's
/// own.
fn discard_opt_enabled(fs: &Fs, ca: &c::bch_dev) -> bool {
    let on = if fs.flag(c::bch_fs_flags::BCH_FS_discard_mount_opt_set) {
        fs.opts.discard
    } else {
        ca.mi.get().discard
    };
    on != 0
}

impl c::bch_fs_discards {
    /// Whether a discard of @bucket could be issued, with none in flight for
    /// it and room on its device.
    fn check(&self, bucket: c::bpos, fastpath: bool) -> Result<(), NotAdded> {
        if self.in_flight.lock().contains(&bucket_to_u64(bucket)) {
            return Err(NotAdded::Exists);
        }
        let fs = self.container();
        let rcu = read_lock();
        match fs.dev_rcu(bucket.inode as u32, &rcu) {
            Some(ca) if !ca.discard_slot_free(fastpath) =>
                Err(NotAdded::Full(fs.err(bch_errcode::BCH_ERR_max_discards_in_flight))),
            _ => Ok(()),
        }
    }
}

/// One more for the fast path, so the discard worker can't keep it from its
/// own.
fn dev_in_flight_max(fastpath: bool) -> u8 {
    DEV_IN_FLIGHT_MAX + u8::from(fastpath)
}

impl c::bch_dev {
    fn discard_slot_free(&self, fastpath: bool) -> bool {
        self.discards_in_flight.load(Ordering::Relaxed) < dev_in_flight_max(fastpath)
    }

    /// A discard in flight, if there's room.
    fn discard_slot_get(&self, fastpath: bool) -> bool {
        let max = dev_in_flight_max(fastpath);
        self.discards_in_flight
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| (n < max).then_some(n + 1))
            .is_ok()
    }

    /// One done: from its endio.
    fn discard_slot_put(&self) {
        let n = self.discards_in_flight.fetch_sub(1, Ordering::Release);
        assert!(n != 0, "discards in flight on {} gone below zero", self.name().to_str().unwrap_or("?"));
        futex_wake(core::ptr::from_ref(&*self.discards_in_flight).cast(), usize::MAX);
    }

    /// Sleep until there's room for a discard - the transaction's locks
    /// dropped: what's waited for is IO.
    fn discard_slot_wait(&self, trans: &BtreeTrans<'_>, fastpath: bool) {
        loop {
            let n = self.discards_in_flight.load(Ordering::Acquire);
            if n < dev_in_flight_max(fastpath) {
                return;
            }
            trans.unlock_long();
            let Ok(()) = futex_wait(&*self.discards_in_flight, n, Sleep::IDLE);
        }
    }
}

impl Discard {
    /// @bucket claimed for a discard by @ca, and a slot on it - unless one's
    /// in flight for it, or @ca has as many as it may.
    fn claim(ca: DevIoRef<c::bch_dev_write_ref>, bucket: c::bpos, fastpath: bool) -> Result<Self, NotAdded> {
        let fs = ca.fs();
        let dev_bucket = bucket_to_u64(bucket);
        let mut claimed = fs.discards.in_flight.lock();

        if claimed.contains(&dev_bucket) {
            return Err(NotAdded::Exists);
        }
        if !ca.discard_slot_get(fastpath) {
            return Err(NotAdded::Full(fs.err(bch_errcode::BCH_ERR_max_discards_in_flight)));
        }
        if claimed.push(dev_bucket, GFP_KERNEL).is_err() {
            ca.discard_slot_put();
            return Err(NotAdded::Full(BchError::from(c::ENOMEM)));
        }
        drop(claimed);
        Ok(Discard { ca, dev_bucket })
    }
}

impl Drop for Discard {
    fn drop(&mut self) {
        self.ca.fs().discards.in_flight.lock().retain(|&mut b| b != self.dev_bucket);
    }
}

struct DiscardEndio;

impl Endio<BioBox<DiscardBio>> for DiscardEndio {
    fn endio(mut bio: BioBox<DiscardBio>) {
        let this = bio.as_mut().project();
        let done = this.done.take().expect("a discard bio, with its Gather ref");
        let d = this.discard.take().expect("a discard bio, with its discard");
        drop(bio);

        // while its io ref is ours: once put, it may be reaped
        d.ca.discard_slot_put();
        done.put(d);
    }
}

impl Discard {
    /// Issue it, to be put to its worker's Discards by @done.
    fn submit(self, fs: &Fs, done: GatherRef<KVVec<Discard>>) {
        let ca = &self.ca;
        let bdev = ca.disk_sb.bdev;
        let sector = ca.bucket_to_sector(u64_to_bucket(self.dev_bucket).offset);
        let size = u32::from(ca.mi.get().bucket_size) << 9;
        // as C's GFP_NOIO: what this waits on mustn't be our own IO
        let _noio = MemallocFlags::noio();
        let mut bio = fs.discards.bioset
            .alloc::<BchError, _>(bdev, 0, c::req_op_REQ_OP_DISCARD, |b| pin_init!(DiscardBio {
                done:    Some(done),
                discard: Some(self),
                bio      <- b.init::<BchError>(),
            }? BchError))
            .expect("a discard bio: the mempool waits rather than fail");

        bio.bio_mut().set_sector(sector);
        bio.bio_mut().set_size(size);
        bio.submit::<DiscardEndio>();
    }
}

// ── A run of discards ────────────────────────────────────────────────────

impl<'a> Worker<'a> {
    fn new(g: &'a Discards, s: &'a SeqLock<DiscardState>, fastpath: bool) -> Self {
        Worker { g, done: KVVec::new(), s, fastpath }
    }

    /// Counted, by why: one in flight for it, or none to be had.
    fn refused(&self, n: NotAdded, bucket_size: u64) -> Result<(), BchError> {
        match n {
            NotAdded::Exists => {
                self.s.write(|s| s.eexist += bucket_size);
                Ok(())
            }
            NotAdded::Full(e) => {
                self.s.write(|s| s.eagain += bucket_size);
                Err(e)
            }
        }
    }

    /// Discard @bucket - or, on a device that doesn't discard, mark it free -
    /// unless something says not yet, which is counted.
    fn one_bucket(&self, t: &TransAttempt<'_, '_>, bucket: c::bpos, bucket_size: u64) -> Result<(), BchError> {
        let fs = t.fs();

        if dev_bucket_nouse(fs, bucket) {
            return Ok(());
        }
        if let Err(n) = fs.discards.check(bucket, self.fastpath) {
            return self.refused(n, bucket_size);
        }

        let mut iter = BtreeIter::new(t, c::btree_id::alloc, bucket, BtreeIterFlags::CACHED);
        let k = iter.peek_slot(t)?.expect("a slot always has a key");
        let a = t.alloc_to_v4_mut(k)?;
        let (seq_empty, data_type) = (a.v().journal_seq_empty, a.v().data_type);
        let (flushed, rewind_ondisk) = (fs.journal.flushed_seq_ondisk.get(), fs.journal.rewind_seq_ondisk.get());

        // Why not yet, if not: the count it goes in
        let skip: Option<fn(&mut DiscardState) -> &mut u64> = if self.fastpath && seq_empty != 0 {
            // Queued to the fast path, but emptied again since by a transaction
            // whose journal entry needs a flush
            Some(|s| &mut s.need_journal_commit)
        } else if seq_empty > flushed {
            Some(|s| &mut s.need_journal_commit)
        } else if seq_empty >= rewind_ondisk {
            Some(|s| &mut s.need_rewind_advance)
        } else if data_type != c::bch_data_type::BCH_DATA_need_discard.0 as u8 {
            // an expected race: the write buffer
            Some(|s| &mut s.bad_data_type)
        } else if fs.bucket_is_open_safe(bucket.inode as u32, bucket.offset) {
            // checked having looked at the alloc key, and locked it
            Some(|s| &mut s.open)
        } else {
            None
        };
        if let Some(count) = skip {
            self.s.write(|s| *count(s) += bucket_size);
            return Ok(());
        }

        let Some(ca) = fs.dev_get_ioref::<c::bch_dev_write_ref>(bucket.inode as u32) else {
            self.s.write(|s| s.not_rw += bucket_size);
            return Ok(());
        };

        if !discard_opt_enabled(fs, &ca) ||
           bdev_max_discard_sectors(ca.disk_sb.bdev()) == 0 ||
           fs.opts.nochanges != 0 {
            return self.mark_free_key(t, &iter, a);
        }

        let issued = match self.g.try_get() {
            Ok(r) => Discard::claim(ca, bucket, self.fastpath).map(|d| (r, d)),
            Err(_) => Err(NotAdded::Full(BchError::from(c::ENOMEM))),
        };
        match issued {
            Ok((r, d)) => {
                t.unlock();
                d.submit(fs, r);
                self.s.write(|s| s.discarded += bucket_size);
                Ok(())
            }
            Err(n) => self.refused(n, bucket_size),
        }
    }

    /// @a, its bucket discarded or on a device that doesn't discard, committed
    /// free - the one path a bucket takes to free from need_discard.
    fn mark_free_key<'t>(&self, t: &TransAttempt<'_, 't>, iter: &BtreeIter<'_>, mut a: TransBkey<'_, 't, c::AllocV4>)
        -> Result<(), BchError>
    {
        let fs = t.fs();

        if a.v().data_type != c::bch_data_type::BCH_DATA_need_discard.0 as u8 {
            let mut msg = LogMsg::new(fs);
            prt!(msg, "Discarded bucket that is no longer BCH_DATA_need_discard!\n{}", a.as_sc().to_text(fs));
            fs.emergency_read_only(&mut msg);
            return fs.throw(bch_errcode::BCH_ERR_emergency_ro);
        }

        let orig = BkeyBuf::from_key(a.as_sc().untyped());

        let v = a.v_mut();
        // kept in step for downgrades: alloc_data_type() no longer reads it
        v.set_need_discard(false);
        v.journal_seq_nonempty = 0;
        v.journal_seq_empty = 0;
        v.set_data_type(data_type::free);

        t.update(iter, &a, UpdateTriggerFlags::IS_DISCARD)?;
        t.commit(None, CommitOpts::new().watermark(Watermark::RECLAIM)
                 .flags(CommitFlags::NO_CHECK_RW | CommitFlags::NO_ENOSPC))?;

        let orig = BkeySC::from(&orig);
        if self.fastpath {
            fs.event_inc_trace(events::bucket_discard_fast, |out| prt!(out, "{}", orig.to_text(fs)));
        } else {
            fs.event_inc_trace(events::bucket_discard, |out| prt!(out, "{}", orig.to_text(fs)));
        }
        let bucket_size = dev_bucket_size(fs, iter.pos().inode);
        self.s.write(|s| s.committed += bucket_size);
        Ok(())
    }

    /// Mark free the buckets of @done's discards - each one's claim and io ref
    /// dropped once its bucket's marked.
    fn mark_free(&self, trans: &BtreeTrans<'_>, done: impl IntoIterator<Item = Discard>) -> Result<(), BchError> {
        let mut ret = Ok(());
        for d in done {
            let bucket = u64_to_bucket(d.dev_bucket);
            ret = lockrestart_do(trans, |t| {
                let mut iter = BtreeIter::new(t, c::btree_id::alloc, bucket,
                                              BtreeIterFlags::CACHED | BtreeIterFlags::INTENT);
                let k = iter.peek_slot(t)?.expect("a slot always has a key");
                let a = t.alloc_to_v4_mut(k)?;
                self.mark_free_key(t, &iter, a)
            }).and(ret);
        }
        ret
    }

    /// Mark free the buckets of the discards done so far. Without the memory
    /// to take them, they're left for finish().
    fn reap(&mut self, trans: &BtreeTrans<'_>) -> Result<(), BchError> {
        let mut done = core::mem::take(&mut self.done);
        let ret = match self.g.take(&mut done) {
            Ok(())  => self.mark_free(trans, done.drain_all()),
            Err(_)  => Ok(()),
        };
        self.done = done;
        ret
    }

    /// Wait out the discards in flight, and mark free the rest's buckets.
    fn finish(self, trans: &BtreeTrans<'_>) -> Result<(), BchError> {
        trans.unlock_long();
        self.mark_free(trans, self.g.wait_sync())
    }
}

// ── The discard worker ───────────────────────────────────────────────────

/// Free space short: release buckets for discard - advancing the journal's
/// rewind seq past them, from @start, where the discard walk stopped - by
/// each device's own shortfall, so one device's pile-up isn't masked by the
/// rest's room. And whether to flush the journal, or the write buffer, where
/// need_discard updates may still be. The figures, and how the walk went.
fn calculate_discard_sectors_to_release(trans: &BtreeTrans<'_>, s: &DiscardState, start: c::bpos)
    -> (DiscardRelease, Result<(), BchError>)
{
    let fs = trans.fs();
    let mut r = DiscardRelease::default();

    {
        let rcu = read_lock();
        for ca in fs.rw_devs_rcu(&rcu) {
            let u = ca.usage_read_fast();
            let sectors = u64::from(ca.mi.get().bucket_size);
            let buckets = |t: c::bch_data_type| u.buckets[t.0 as usize];

            let pending  = sectors * buckets(c::bch_data_type::BCH_DATA_need_discard);
            let free     = sectors * buckets(c::bch_data_type::BCH_DATA_free);
            let reserve  = sectors * ca.buckets_reserved(c::bch_watermark::BCH_WATERMARK_stripe);
            let capacity = sectors * ca.mi.get().nbuckets;
            let buffer   = capacity * u64::from(fs.opts.journal_rewind_discard_buffer_percent) / 100;
            let buffer_clamped = (buffer as i64).min((free as i64 - reserve as i64 * 4).max(0));
            let release  = pending as i64 - buffer_clamped;

            r.pending_total  += pending;
            r.free           += free;
            r.reserve        += reserve;
            r.buffer         += buffer;
            r.buffer_clamped += buffer_clamped as u64;
            if release > 0 {
                r.release += release;
            }
        }
    }

    // u64s, wrapping as C's: a bucket retried after its device was at its
    // limit is seen once, but may be counted skipped twice
    let seen = s.seen.wrapping_sub(s.bad_data_type).wrapping_sub(s.not_rw);
    let short = r.free < r.reserve * 2;
    r.flush_wb = seen.wrapping_mul(2) <= r.pending_total && short;

    if r.release <= 0 {
        return (r, Ok(()));
    }

    let mut release = r.release;
    let mut iter = BtreeIter::new(trans, c::btree_id::need_discard, start, BtreeIterFlags::empty());
    let walked = iter.for_each(trans, |_, _, k| {
        let journal_seq = k.k.p.inode;
        let bucket = u64_to_bucket(k.k.p.offset);

        if fs.bucket_is_open_safe(bucket.inode as u32, bucket.offset) {
            return Ok(LoopFlow::Continue);
        }

        r.new_rewind_seq = r.new_rewind_seq.max(journal_seq + 1);
        if short && fs.journal.flushed_seq_ondisk.get() < fs.journal_cur_seq() {
            r.flush_journal = true;
        }

        release -= dev_bucket_size(fs, bucket.inode) as i64;
        Ok(if release <= 0 { LoopFlow::Break(()) } else { LoopFlow::Continue })
    });

    if short && r.new_rewind_seq > fs.journal.rewind_seq.get() {
        r.flush_journal = true;
    }
    (r, walked)
}

fn do_discards(fs: &Fs) {
    let d = &fs.discards;
    let trans = crate::btree_trans!(fs);
    let s = &d.s;
    let mut flushed_wb = 0;

    let ret = loop {
        s.write(|s| *s = DiscardState::default());
        let mut last = None;

        stack_pin_init!(let g = Discards::new());
        let mut w = Worker::new(&g, s, false);

        // need_discard, by the journal seq each bucket was emptied at, up to
        // what the journal's flushed and can no longer rewind past
        let mut iter = BtreeIter::new(&trans, c::btree_id::need_discard, POS_MIN, BtreeIterFlags::empty());
        let mut ret = iter.for_each(&trans, |t, _, k| {
            let journal_seq = k.k.p.inode;
            let p = k.k.p;
            let bucket = u64_to_bucket(p.offset);
            let bucket_size = dev_bucket_size(fs, bucket.inode);

            let j = &fs.journal;
            if journal_seq >= j.rewind_seq_ondisk.get().min(j.flushed_seq_ondisk.get() + 1) {
                return Ok(LoopFlow::Break(()));
            }

            if last != Some(p) {
                s.write(|s| s.seen += bucket_size);
            }
            last = Some(p);

            // Refused for its device's limit: the bucket again, once there's
            // room - unless the device is going, when it's left
            let full = match w.one_bucket(t, bucket, bucket_size) {
                Ok(()) => false,
                Err(e) if e.matches(bch_errcode::BCH_ERR_max_discards_in_flight) => true,
                // restarts too: this bucket again
                Err(e) => return Err(e),
            };
            let ca = full.then(|| fs.dev_get_ioref::<c::bch_dev_write_ref>(bucket.inode as u32)).flatten();

            // Reaped as we go: each holds its claim, and its io ref, till its
            // bucket's marked free
            t.ignore_restarts(|trans| {
                if let Some(ca) = &ca {
                    ca.discard_slot_wait(trans, false);
                }
                w.reap(trans)
            })?;
            Ok(if ca.is_some() { LoopFlow::ContinueFrom(p) } else { LoopFlow::Continue })
        });

        ret = w.finish(&trans).and(ret);

        // Calculated whatever the pass's result, for to_text; acted on only
        // if it went well
        let start = last.unwrap_or(POS_MIN);
        let (r, walked) = calculate_discard_sectors_to_release(&trans, &s.read(), start);
        s.write(|s| s.r = r);
        if let Err(e) = ret.and(walked) {
            break Err(e);
        }

        if r.new_rewind_seq != 0 {
            fs.journal_advance_rewind_seq(r.new_rewind_seq);
        }

        let mut again = false;
        if r.flush_journal {
            trans.unlock_long();
            let start = local_clock();
            let ret = fs.journal_flush();
            fs.times[c::bch_time_stats::BCH_TIME_blocked_discard_journal_flush as usize].update(start);
            if let Err(e) = ret {
                break Err(e);
            }
            again = true;
        }

        // need_discard may be empty while buckets are short: the alloc
        // trigger's updates to it go through the write buffer
        if r.flush_wb && flushed_wb < 2 {
            match trans.write_buffer_flush_sync() {
                Err(e) if !is_restart(&e) => break Err(e),
                _ => {}
            }
            again = true;
            flushed_wb += 1;
        }

        fs.event_inc_trace(events::bucket_discard_worker, |out| {
            prt!(out, "ret 0\n");
            fs.discards_to_text(out);
        });

        if !again {
            break Ok(());
        }
    };

    if let Err(e) = &ret {
        fs.event_inc_trace(events::bucket_discard_worker, |out| {
            prt!(out, "ret {}\n", e.msg());
            fs.discards_to_text(out);
        });
        if !e.matches(c::EROFS) {
            let _ = bch_err_fn!(fs, ret);
        }
    }
}

pub(crate) fn do_discards_work(d: &c::bch_fs_discards) {
    do_discards(d.container())
}

impl Fs {
    /// Kick the discard worker - unless the filesystem is going read-only;
    /// it takes its own write ref.
    pub fn do_discards_async(&self) {
        self.discards.work.queue(self.write_ref_wq());
    }
}

#[c_export]
pub fn bch2_do_discards_async(c: &Opaque<c::bch_fs>) {
    Fs::from_c(c).do_discards_async()
}

/// Going read-only, with free space short on a device with buckets waiting
/// for discard: discard them now, before the allocator stops.
#[c_export]
pub fn bch2_do_discards_going_ro(c: &Opaque<c::bch_fs>) {
    let fs = Fs::from_c(c);
    let must = fs.rw_devs_rcu(&read_lock()).any(|ca| {
        let u = ca.usage_read_fast();
        u.buckets[c::bch_data_type::BCH_DATA_free.0 as usize] <
            ca.buckets_reserved(c::bch_watermark::BCH_WATERMARK_stripe) * 4 &&
        u.buckets[c::bch_data_type::BCH_DATA_need_discard.0 as usize] != 0
    });
    if must {
        do_discards(fs);
    }
}

// ── Fast discards ────────────────────────────────────────────────────────

/// A device's fast discard work: its queued buckets, each discarded and
/// marked free once its discard completes.
pub(crate) fn do_discards_fast(ca: &c::bch_dev) {
    let fs = ca.fs();
    let s = SeqLock::new(DiscardState::default());
    let trans = crate::btree_trans!(fs);
    stack_pin_init!(let g = Discards::new());
    let mut w = Worker::new(&g, &s, true);

    let mut ret = Ok(());
    loop {
        // The lock dropped with the bucket popped - not held through the
        // loop, as a while let's would be: the alloc trigger takes it to
        // queue a bucket, in a commit holding btree locks this would wait on
        let Some(bucket) = ca.discard_fast.lock().pop() else { break };
        let bucket = pos(ca.dev_idx.into(), bucket);
        ret = loop {
            match lockrestart_do(&trans, |t| w.one_bucket(t, bucket, ca.mi.get().bucket_size.into())) {
                Err(e) if e.matches(bch_errcode::BCH_ERR_max_discards_in_flight) => ca.discard_slot_wait(&trans, true),
                r => break r,
            }
        };
        ret = w.reap(&trans).and(ret);
        if ret.is_err() {
            break;
        }
    }
    ret = w.finish(&trans).and(ret);

    fs.event_inc_trace(events::bucket_discard_fast_worker, |out| {
        prt!(out, "dev {}: ret {}\n", ca.name().to_str().unwrap_or("?"),
             ret.as_ref().map_or_else(|e| e.msg(), |()| "0"));
        discard_state_to_text(out, &s.read());
    });
}

/// Queue @bucket of @ca for fast discard, and kick its work: the alloc
/// trigger's, for a bucket emptied by a transaction that needed no flush -
/// or the open bucket's close, if it was emptied while open.
pub(crate) fn fast_discard_bucket_add(ca: &c::bch_dev, bucket: u64) {
    if ca.discard_fast.lock().push(bucket, GFP_KERNEL).is_err() {
        return;
    }
    if dev_is_rw(ca) {
        ca.discard_fast_work.queue(ca.fs().write_ref_wq());
    }
}

/// As bch2_dev_get_ioref() has it for a write ref: work queued holding one is
/// for a device that's rw.
fn dev_is_rw(ca: &c::bch_dev) -> bool {
    ca.mi.get().state as u32 == c::bch_member_state::BCH_MEMBER_STATE_rw.0
}

// ── Invalidates ──────────────────────────────────────────────────────────

/// How many buckets @ca should invalidate: what it's short of a 32nd of its
/// buckets free past what's held back - at most its cached buckets.
///
/// Held back is the reserve the free space accounting takes off,
/// reserved_sectors - the device's share of the copygc reserve,
/// gc_reserve_percent (bch2_recalc_capacity()) - or the stripe watermark's,
/// where that's more. Invalidating is the cheapest way to free space, nothing
/// moved: stopping short of reserved_sectors leaves no space writable now,
/// with cached buckets sitting unreclaimed.
impl c::bch_dev {
    pub fn should_invalidate_buckets(&self, u: &c::bch_dev_usage) -> u64 {
        let reserved = self.reserved_sectors.get().div_ceil(self.mi.get().bucket_size.into())
            .max(self.buckets_reserved(c::bch_watermark::BCH_WATERMARK_stripe));
        let want_free = reserved + (self.mi.get().nbuckets >> 5);
        let free = u.buckets[c::bch_data_type::BCH_DATA_free.0 as usize];
        let cached = u.buckets[c::bch_data_type::BCH_DATA_cached.0 as usize] as i64;
        (want_free as i64 - free as i64).clamp(0, cached) as u64
    }
}

/// Drop @ca's pointers from what backpointer @bp points to - its data's lost,
/// recorded, if that leaves it unreadable.
fn invalidate_one_bp(t: &TransAttempt<'_, '_>, ca: &c::bch_dev, bp: BkeySC<'_, c::Backpointer>,
                     last_flushed: WbFlush<'_>) -> Result<(), BchError>
{
    let fs = t.fs();
    let mut iter = BtreeIter::uninit();
    let Some((iter, k)) = backpointer_get_key(t, bp, &mut iter, BtreeIterFlags::empty(), last_flushed)? else {
        return Ok(());
    };
    let Some(k) = k.arm::<Ptrs>() else { return Ok(()) };

    let mut n = t.bkey_make_mut(iter, k, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)?;
    n.drop_device_noerror(fs, ca.dev_idx.into());
    let mut n: TransBkey<'_, '_> = n.into();

    if !n.as_sc().can_read(fs) {
        n.set_error(fs, c::bch_key_type_errors::KEY_TYPE_ERROR_device_removed);
        damage::record_data_loss(t, c::btree_id(bp.v().btree_id.into()), n.k().p, id::data_lost_device_removed)?;
    }
    Ok(())
}

/// Invalidate @bucket of generation @gen: drop @ca's pointers from what each
/// of its backpointers of that generation point to.
///
/// At the btree_copygc watermark: invalidating frees space and writes no
/// data - it only shrinks keys - so it mustn't wait behind a reserve for a
/// btree node. It's what copygc's btree node allocations get
/// (btree_update_set_watermark_hipri()), and no bucket reserve is below it.
fn invalidate_one_bucket_by_bps(trans: &BtreeTrans<'_>, ca: &c::bch_dev, bucket: c::bpos, gen: u8,
                                last_flushed: &mut WbMaybeFlush) -> Result<(), BchError>
{
    let res = DiskReservation::new(trans.fs());
    let bp_range = BackpointerPos::bucket_range(ca, bucket);

    BtreeIter::new(trans, c::btree_id::backpointers, *bp_range.start(), BtreeIterFlags::empty())
        .for_each_max_commit(trans, *bp_range.end(), BtreeIterFlags::empty(), Some(&res),
                             CommitOpts::new().watermark(Watermark::BTREE_COPYGC).flags(CommitFlags::NO_ENOSPC),
                             |t, _, k| {
            let Some(bp) = k.arm::<c::Backpointer>() else { return Ok(()) };
            if bp.v().bucket_gen != gen {
                return Ok(());
            }
            invalidate_one_bp(t, ca, bp, last_flushed.borrow())
        })
}

/// Invalidate the bucket lru entry @lru_k is for, if it's still cached and
/// still due - and count it off @nr.
fn invalidate_one_bucket(t: &TransAttempt<'_, '_>, ca: &c::bch_dev, lru_k: BkeySC<'_>,
                         last_flushed: &mut WbMaybeFlush, nr: &mut i64) -> Result<(), BchError>
{
    let fs = t.fs();
    let lru_pos = lru_k.k.p;
    let bucket = u64_to_bucket(lru_pos.offset);

    if fs.dev_bucket_tryget_noerror(bucket).is_none() {
        if fsck_err!(t, id::lru_entry_to_invalid_bucket,
                     "lru key points to nonexistent device:bucket {}:{}", { bucket.inode }, { bucket.offset })? {
            return t.bit_mod_buffered(c::btree_id::lru, lru_pos, false);
        }
        return Ok(());
    }

    if fs.bucket_is_open_safe(bucket.inode as u32, bucket.offset) {
        return Ok(());
    }

    let mut alloc_iter = BtreeIter::new(t, c::btree_id::alloc, bucket, BtreeIterFlags::CACHED);
    let alloc_k = alloc_iter.peek_slot(t)?.expect("a slot always has a key");
    let a = alloc_to_v4(alloc_k);

    // A harmless race: the write buffer
    if LruPos::from_bpos(lru_pos).time != a.lru_idx_read() {
        return Ok(());
    }

    // lru_idx_read() is nonzero only for a cached bucket, and lru_validate()
    // refuses an entry at time 0
    assert_eq!(a.data_type, c::bch_data_type::BCH_DATA_cached.0 as u8, "a read lru entry for a bucket not cached");
    assert_eq!({ a.dirty_sectors }, 0, "a read lru entry for a bucket with dirty data");

    if a.cached_sectors == 0 {
        let _ = bch_err_msg!(fs, t.ignore_restarts(|trans| {
            check_one_bucket_backpointer_mismatch(trans, ca, bucket.offset, true, last_flushed.borrow())
        }), "checking backpointers of bucket {}:{}", { bucket.inode }, { bucket.offset });
        return Ok(());
    }

    let orig = BkeyBuf::from_key(alloc_k);
    t.ignore_restarts(|trans| invalidate_one_bucket_by_bps(trans, ca, bucket, a.generation, last_flushed))?;

    let orig = BkeySC::from(&orig);
    fs.event_inc_trace(events::bucket_invalidate, |out| prt!(out, "{}", orig.to_text(fs)));
    *nr -= 1;
    Ok(())
}

/// A device's invalidate work: its read lru from the current time, wrapping
/// round once, its cached buckets invalidated until it isn't short.
pub(crate) fn do_invalidates_work(ca: &c::bch_dev) {
    let fs = ca.fs();
    let trans = crate::btree_trans!(fs);
    let ret = (|| -> Result<(), BchError> {
        let mut last_flushed = WbMaybeFlush::new().map_err(|_| BchError::from(c::ENOMEM))?;
        let _ = trans.write_buffer_tryflush();

        let mut nr = ca.should_invalidate_buckets(&ca.usage_read_fast()) as i64;
        let dev = u16::from(ca.dev_idx);
        let end = LruPos::new(dev, c::LRU_TIME_MAX, u64::MAX).to_bpos();
        let now = (fs.current_io_time(READ) + u64::from(u32::MAX)) & c::LRU_TIME_MAX;

        // from now, then round once from the start
        for start in [now, 0] {
            if nr == 0 {
                break;
            }
            BtreeIter::new(&trans, c::btree_id::lru, LruPos::new(dev, start, 0).to_bpos(), BtreeIterFlags::empty())
                .for_each_max(&trans, end, |t, _, k| {
                    invalidate_one_bucket(t, ca, k, &mut last_flushed, &mut nr)?;
                    if nr == 0 {
                        nr = ca.should_invalidate_buckets(&ca.usage_read_fast()) as i64;
                        if nr == 0 {
                            return Ok(LoopFlow::Break(()));
                        }
                    }
                    last_flushed.inc();
                    Ok(LoopFlow::Continue)
                })?;
        }
        Ok(())
    })();
    let _ = bch_err_msg!(fs, ret, "invalidating buckets on {}", ca.name().to_str().unwrap_or("?"));
}

impl c::bch_dev {
    /// Kick its invalidate work - unless it isn't rw, or the filesystem is
    /// going read-only.
    pub fn do_invalidates(&self) {
        if dev_is_rw(self) {
            self.invalidate_work.queue(self.fs().write_ref_wq());
        }
    }
}

impl Fs {
    pub fn do_invalidates(&self) {
        for ca in self.members() {
            ca.do_invalidates();
        }
    }
}

#[c_export]
pub fn bch2_do_invalidates(c: &Opaque<c::bch_fs>) {
    Fs::from_c(c).do_invalidates()
}
