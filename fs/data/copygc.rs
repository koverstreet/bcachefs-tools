// SPDX-License-Identifier: GPL-2.0
// Copyright 2012 Google, Inc.

/* DOC(copygc)
 *
 * As a copy-on-write filesystem, bcachefs never overwrites data in place.
 * Random overwrites leave buckets partially empty — the old data is obsolete
 * but still occupies disk space until the bucket is reclaimed. The copying
 * garbage collector (copygc) handles this: it finds the most fragmented
 * buckets, relocates their remaining live data, and frees the buckets for
 * reuse. This is automatic and continuous.
 *
 * Performance near full: copygc needs free space to relocate data into, so a
 * portion of free space is reserved exclusively for it (`copygc_reserve`, 8%
 * by default, configurable 5-20%). Normal writes cannot dip into this reserve.
 * As the filesystem fills beyond the reserve threshold, write latency
 * increases because new writes must wait for copygc to free space first. This
 * is the primary reason to avoid running a bcachefs filesystem above ~90%
 * capacity under write-heavy workloads.
 *
 * A fragmentation LRU btree tracks bucket fill levels so copygc can
 * efficiently find the worst buckets without scanning the entire allocation
 * state. Copygc reads live data from selected buckets (located via
 * backpointers), writes it to new buckets, and updates extent pointers
 * atomically.
 */

//! The copying garbage collector's thread, and what the allocator asks of it.
//!
//! A pass: the devices over their fragmentation allowance (dev_list(),
//! neediest first); a batch of buckets off their fragmentation lrus, one per
//! device round robin (get_buckets()) - or off the stripe lru, when stripes
//! are emptier than any bucket we'd take (should_do_ec()); each bucket
//! evacuated, its live data moved off through its backpointers by move.c.
//! Then a sleep on the write io clock, until the nearest device will need us,
//! or a kick: the allocator's, when it's blocked on space we can free.
//!
//! Buckets in flight. An evacuation returns once its reads are issued; each
//! move holds a count on the bucket's move_bucket - all C sees of its InFlight
//! - until its update is done. Until then the bucket isn't picked again, and
//! counts in its device's copygc_in_flight, which the allocator reads
//! (can_make_progress()). An InFlight holds its place in the in-flight set - a
//! cuckoo Registration - and its device's count: dropping it ends both, so a
//! batch cut short, the oldest done and the thread exiting are drops. The
//! batch, then the FIFO, own them - the FIFO in the order evacuated, dropping
//! them from its front as they finish; all three are the thread's, so the
//! set's read by nothing else, and an InFlight's freed without a grace
//! period.
//!
//! A run ends with running cleared, run_count bumped and run_done woken - an
//! EventCount, where C had a waitqueue: what waits for a run is in Rust, its
//! condition with it, and C calls in. The wake's release orders the fields
//! for a waiter, whose snapshot acquires.

use crate::accounting;
use crate::alloc::background::alloc_to_v4;
use crate::alloc::buckets::u64_to_bucket;
use crate::alloc::lru::{LruId, LruPos};
use crate::btree::bkey::{pos, BkeyV};
use crate::btree::iter::{lockrestart_do, BtreeIter, BtreeIterFlags, BtreeTrans, LoopFlow, TransAttempt};
use crate::c;
use crate::c::bch_errcode;
use crate::data::ec::trigger::data_blocks;
use crate::data::moving::{MoveStats, MovingCtxt};
use crate::errcode::{ret_to_c, BchError};
use crate::fs::{writepoint_ptr, Fs};
use crate::sb::counters::events;
use crate::types::Fifo;
use crate::util::alloc::{flags::GFP_KERNEL, kbox_pin_init, KBox, KVVec};
use crate::util::alloc::AllocError;
use crate::util::cuckoo::{Cuckoo, Entry, Registration};
use crate::util::kernel::{cond_resched, WRITE};
use crate::util::kthread::{Kthread, KthreadSelf};
use crate::util::rcu::read_lock;
use crate::util::sleep::{sleep_until, Sleep};
use crate::util::sort::heapsort_by;
use crate::util::workqueue::{Workqueue, WqFlags};
use crate::util::Printbuf;
use crate::{bch_err_msg, fs_fatal_error};
use core::borrow::Borrow;
use core::ffi::c_int;
use core::hash::{Hash, Hasher};
use core::pin::Pin;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicI32, Ordering::{Acquire, Relaxed}};
use cstruct_macros::c_export;
use pin_init::{pin_data, pin_init, PinInit};

// ── Buckets in flight ────────────────────────────────────────────────────

/// A bucket's count in its device's copygc_in_flight, while it's in flight:
/// copygc_in_flight_add(). A device gone meanwhile isn't counted.
struct DevInFlight<'f> {
    fs:  &'f Fs,
    dev: u32,
}

impl<'f> DevInFlight<'f> {
    fn add(&self, d: i32) {
        let rcu = read_lock();
        if let Some(ca) = self.fs.dev_rcu(self.dev, &rcu) {
            ca.copygc_in_flight.fetch_add(d, Relaxed);
        }
    }

    fn new(fs: &'f Fs, dev: u32) -> Self {
        let d = DevInFlight { fs, dev };
        d.add(1);
        d
    }
}

impl Drop for DevInFlight<'_> {
    fn drop(&mut self) {
        self.add(-1);
    }
}

#[pin_data]
struct InFlight<'f> {
    /// First, so it's out of the set before the rest goes
    #[pin]
    reg:  Registration<c::move_bucket_key, InFlight<'f>>,
    _dev: DevInFlight<'f>,
    /// What the moves count on
    b:    c::move_bucket,
}

type InFlightSet<'f> = Cuckoo<Entry<InFlight<'f>>>;

impl<'f> InFlight<'f> {
    fn new<'w>(fs: &'f Fs, set: &'w InFlightSet<'f>, key: c::move_bucket_key)
        -> impl PinInit<Self, AllocError> + use<'f, 'w>
    {
        pin_init!(&this in InFlight::<'f> {
            _dev: DevInFlight::new(fs, key.bucket.inode as u32),
            b:    c::move_bucket { count: AtomicI32::new(0) },
            // SAFETY: the set's the thread's: only it reads the set, so
            // Entry::new()'s grace period is none, and it drops every
            // InFlight before the set goes. This is the first field,
            // initialized last, so the rest's there when it's hashed.
            reg <- unsafe { Registration::new(set, key, this) },
        }? AllocError)
    }

    fn key(&self) -> &c::move_bucket_key {
        self.reg.key()
    }

    fn done(&self) -> bool {
        self.b.count.load(Acquire) == 0
    }
}

impl Hash for InFlight<'_> {
    fn hash<H: Hasher>(&self, h: &mut H) {
        self.key().hash(h)
    }
}

impl PartialEq for InFlight<'_> {
    fn eq(&self, o: &Self) -> bool {
        self.key() == o.key()
    }
}

impl Eq for InFlight<'_> {}

impl Borrow<c::move_bucket_key> for InFlight<'_> {
    fn borrow(&self) -> &c::move_bucket_key {
        self.key()
    }
}

type Batch<'f> = KVVec<Pin<KBox<InFlight<'f>>>>;

// ── Device wait amounts ──────────────────────────────────────────────────

/*
 * Free space thresholds, as a percentage of capacity. The wait below is
 * free * 5 - capacity: that scale is also the io clock sleep amount, so it
 * stays as written rather than being derived from the percentage.
 */
const COPYGC_FREE_THRESHOLD_PCT: i64 = 20;
const EC_REUSE_FREE_THRESHOLD_PCT: i64 = COPYGC_FREE_THRESHOLD_PCT * 2;

const _: () = assert!(COPYGC_FREE_THRESHOLD_PCT * 5 == 100);

/// How much io (in sectors, by the write io clock) until @ca will need
/// copygc: <= 0 means it needs it now, and the magnitude is how far past its
/// fragmented-space allowance it is - the sort key for picking which device
/// needs copygc the most. And whether free space is under the looser stripe
/// reuse threshold - reported from here because a positive wait could be
/// either of the two paths below, and doesn't say which.
///
/// mark_lock held (read), for the dev_leaving accounting read - taken
/// outside any rcu read section, as mark_lock can block.
///
/// The allowance at the limit - when the device is full - is the space we
/// reserved in bch2_recalc_capacity; we can't have more than that amount of
/// disk space stranded due to fragmentation and store everything we have
/// promised to store. But we don't want to be running copygc unnecessarily
/// when the device still has plenty of free space - rather, we want copygc to
/// smoothly run every so often and continually reduce the amount of
/// fragmented space as the device fills up - so we increase the allowance by
/// half the current free space.
///
/// In C's arithmetic: sector counts are u64s, made s64s where C's are, and
/// wrapping - a kernel build panics on overflow, and accounting gone wrong
/// mustn't take the kernel with it.
pub(crate) fn dev_wait_amount(ca: &c::bch_dev) -> (i64, bool) {
    // SAFETY: a device's filesystem outlives it
    let fs = unsafe { Fs::borrow_raw(ca.fs) };
    let usage_full = ca.usage_full_read_fast();
    let mut usage = c::bch_dev_usage::default();
    for (b, d) in usage.buckets.iter_mut().zip(&usage_full.d) {
        *b = d.buckets;
    }

    /*
     * Sectors that reconcile is scheduled to move off this device count as
     * free-to-be: a full device whose data is mostly leaving doesn't need
     * copygc, it needs reconcile to run.
     */
    let mut leaving = [0];
    accounting::mem_read_locked(fs, &c::disk_accounting_pos::from_arm(c::bch_acct_dev_leaving {
        dev: ca.dev_idx.into(),
    }), &mut leaving);
    let leaving = (leaving[0] as i64).max(0);

    let bucket_size = u64::from(ca.mi.bucket_size);
    let free = usage.buckets[c::bch_data_type::BCH_DATA_free.0 as usize]
        .wrapping_mul(bucket_size).wrapping_add(leaving as u64) as i64;
    let capacity = ca.mi.nbuckets.wrapping_mul(bucket_size) as i64;

    let low_on_space = free.wrapping_mul(100) <= capacity.wrapping_mul(EC_REUSE_FREE_THRESHOLD_PCT);

    /* Don't start until less than COPYGC_FREE_THRESHOLD_PCT is free: */
    let wait = free.wrapping_mul(5).wrapping_sub(capacity);
    if wait > 0 {
        return (wait, low_on_space);
    }

    let watermark = c::bch_watermark::BCH_WATERMARK_stripe;
    let fragmented_allowed = (ca.free_buckets_at(&usage, watermark).wrapping_add(ca.buckets_reserved(watermark))
        .wrapping_mul(bucket_size).wrapping_add(leaving as u64) >> 1) as i64;
    let fragmented = usage_full.d.iter().enumerate()
        .filter(|&(i, _)| accounting::data_type_movable(c::bch_data_type(i as u32)))
        .fold(0u64, |f, (_, d)| f.wrapping_add(d.fragmented)) as i64;

    (fragmented_allowed.wrapping_sub(fragmented), low_on_space)
}

/// Will copygc run on @ca? The allocator uses this on the blocked path to
/// decide whether to kick copygc and wait for it, or bail: it must be the
/// same criterion copygc uses to build its device list, so the allocator
/// never waits on a copygc run that isn't coming - and never bails when one
/// is.
///
/// Plus buckets copygc already has queued here: the wait amount is a
/// threshold copygc drives to zero, so on a device it is actively working the
/// comparison flips constantly and says nothing about whether space is
/// coming. mark_lock held, as dev_wait_amount().
pub(crate) fn can_make_progress(ca: &c::bch_dev) -> bool {
    ca.copygc_in_flight.load(Relaxed) != 0 || dev_wait_amount(ca).0 <= 0
}

// ── Picking buckets ──────────────────────────────────────────────────────

struct CopygcDev {
    dev:  u32,
    wait: i64,
    pos:  c::bpos,
    /// Its lru exhausted, or it no longer needs copygc
    done: bool,
}

fn fragmentation_lru(dev: u32) -> core::ops::RangeInclusive<LruPos> {
    LruPos::range(LruId::Fragmentation { dev })
}

/// The pass's batch is full: 16 buckets, or a quarter of those in flight.
fn batch_full(batch: &Batch<'_>, in_flight: usize) -> bool {
    batch.len() >= 16usize.max(in_flight / 4)
}

struct Copygc<'f, 'w> {
    fs:        &'f Fs,
    me:        &'w KthreadSelf<'w>,
    ctxt:      MovingCtxt<'w>,
    set:       &'w InFlightSet<'f>,
    in_flight: Fifo<Pin<KBox<InFlight<'f>>>>,
}

impl<'f, 'w> Copygc<'f, 'w> {
    fn trans(&self) -> &BtreeTrans<'w> {
        self.ctxt.trans()
    }

    /// The devices that need copygc now - fragmented space over their
    /// allowance, i.e. wait amount exhausted - into @devs, neediest first.
    /// When there are none, the amount of io (in sectors, by the write io
    /// clock) until the closest device will need it; else 0.
    fn dev_list(&self, devs: &mut KVVec<CopygcDev>) -> Result<u64, BchError> {
        let fs = self.fs;
        let mut wants_space = c::DevsMask::default();
        let mut low_on_space = c::DevsMask::default();
        let mut wait = u64::MAX;

        devs.clear();
        devs.reserve(fs.nr_devices() as usize, GFP_KERNEL)?;

        {
            let _mark = fs.mark_lock().read();
            let rcu = read_lock();
            for ca in fs.rw_devs_rcu(&rcu) {
                let (v, dev_low_on_space) = dev_wait_amount(ca);
                let dev = usize::from(ca.dev_idx);

                /* Over allowance, whether or not we have room to queue it: */
                if v <= 0 {
                    wants_space.set(dev);
                }
                if dev_low_on_space {
                    low_on_space.set(dev);
                }

                /* No allocating under rcu - skip if a device raced in: */
                if v <= 0 && devs.len() < devs.capacity() {
                    let d = CopygcDev { dev: ca.dev_idx.into(), wait: v, pos: c::bpos::default(), done: false };
                    devs.push(d, GFP_KERNEL).expect("within capacity");
                } else if v > 0 {
                    wait = wait.min(v as u64);
                }
            }
        }

        fs.copygc.wants_space.store(&wants_space);
        fs.copygc.low_on_space.store(&low_on_space);

        heapsort_by(&mut devs[..], |l, r| l.wait.cmp(&r.wait));

        Ok(if devs.is_empty() { wait } else { 0 })
    }

    /// In-flight evacuations completing and reconcile moving data off can
    /// bring a device back under its allowance mid batch - pop it off the
    /// list when it no longer needs copygc.
    fn dev_still_needed(&self, dev: u32) -> bool {
        let _mark = self.fs.mark_lock().read();
        let rcu = read_lock();
        self.fs.dev_rcu(dev, &rcu).is_some_and(can_make_progress)
    }

    /// The bucket's generation, if it can be evacuated: on a device we can
    /// write, not being written, its backpointers checked, and still as
    /// fragmented as its lru entry at @time says - U64_MAX, a stripe's
    /// block, any. Why not is counted, each reason an error thrown.
    ///
    /// XXX: we should kill the LRU entry here if the bucket's not valid.
    fn bucket_is_movable(t: &TransAttempt<'_, '_>, bucket: c::bpos, time: u64)
        -> Result<Option<u32>, BchError>
    {
        let fs = t.fs();
        let Ok(ca) = fs.dev_bucket_tryget(bucket) else { return Ok(None) };

        if !ca.is_rw() || !ca.is_online() {
            let _ = fs.err(bch_errcode::BCH_ERR_bucket_not_moveable_dev_not_rw);
            return Ok(None);
        }

        /* Bucket still being written? */
        if fs.bucket_is_open(bucket.inode as u32, bucket.offset) {
            let _ = fs.err(bch_errcode::BCH_ERR_bucket_not_moveable_bucket_open);
            return Ok(None);
        }

        /* We won't be able to evacuate it if there's missing backpointers */
        if ca.bucket_backpointer_mismatch.test(bucket.offset) {
            let _ = fs.err(bch_errcode::BCH_ERR_bucket_not_moveable_bp_mismatch);
            return Ok(None);
        }

        let mut iter = BtreeIter::new(t, c::btree_id::alloc, bucket, BtreeIterFlags::CACHED);
        let k = iter.peek_slot(t)?.expect("a slot always has a key");
        let a = alloc_to_v4(k);
        let lru_idx = a.lru_idx_fragmentation(&ca);

        if lru_idx == 0 || lru_idx > time {
            let _ = fs.err(bch_errcode::BCH_ERR_bucket_not_moveable_lru_race);
            return Ok(None);
        }
        Ok(Some(a.generation.into()))
    }

    /// Add @bucket to @batch, if it's movable and not in flight already:
    /// whether it was. Its side effects - the InFlight, registered and
    /// counted - come after its last btree access, so a restart retrying
    /// the key it was for repeats none of them; and one that retries a
    /// stripe's key finds its earlier blocks in flight.
    fn try_add(&self, t: &TransAttempt<'_, '_>, batch: &mut Batch<'f>, bucket: c::bpos, time: u64)
        -> Result<bool, BchError>
    {
        let Some(generation) = Self::bucket_is_movable(t, bucket, time)? else { return Ok(false) };
        let key = c::move_bucket_key { bucket, generation };

        if self.set.contains(&key) {
            return Ok(false);
        }

        batch.reserve(1, GFP_KERNEL)?;
        let b = kbox_pin_init(InFlight::new(self.fs, self.set, key))?;
        batch.push(b, GFP_KERNEL).expect("reserved");
        Ok(true)
    }

    /// One bucket off @d's fragmentation lru, resuming from where we left
    /// off: d.done if the lru's exhausted.
    fn dev_get_bucket(&self, batch: &mut Batch<'f>, d: &mut CopygcDev) -> Result<(), BchError> {
        let trans = self.trans();
        let end = fragmentation_lru(d.dev).end().to_bpos();
        let mut iter = BtreeIter::new(trans, c::btree_id::lru, d.pos, BtreeIterFlags::empty());

        let added = iter.for_each_max(trans, end, |t, _, k| {
            let lru = LruPos::from_bpos(k.k.p);
            let added = self.try_add(t, batch, u64_to_bucket(lru.target), lru.time)?;
            d.pos = k.k.p.successor();
            Ok(if added { LoopFlow::Break(true) } else { LoopFlow::Continue })
        })?;

        if !added {
            d.done = true;
        }
        Ok(())
    }

    fn get_buckets(&self, batch: &mut Batch<'f>, devs: &mut KVVec<CopygcDev>) -> Result<(), BchError> {
        for d in devs.iter_mut() {
            d.pos  = fragmentation_lru(d.dev).start().to_bpos();
            d.done = false;
        }

        /*
         * Round robin among the devices that need evacuating, one bucket per
         * device per pass, so that no single device monopolizes the batch and
         * every pressured device makes progress:
         */
        loop {
            let mut done = 0;

            for d in devs.iter_mut() {
                if !d.done && !self.dev_still_needed(d.dev) {
                    d.done = true;
                }
                if d.done {
                    done += 1;
                    continue;
                }

                self.dev_get_bucket(batch, d)?;

                if batch_full(batch, self.in_flight.len()) {
                    return Ok(());
                }
            }

            if done >= devs.len() {
                return Ok(());
            }
        }
    }

    /// The stripe at @lru_k's lru entry, if it's still there and its entry's
    /// current - the lru is updated through the write buffer.
    fn lru_stripe<'k, 't>(t: &'k TransAttempt<'_, 't>, iter: &'k mut BtreeIter<'t>, lru_k: c::bpos)
        -> Result<Option<crate::data::ec::trigger::StripeK<'k>>, BchError>
    {
        let lru = LruPos::from_bpos(lru_k);
        iter.set_pos(pos(0, lru.target));
        let s_k = iter.peek_slot(t)?.expect("a slot always has a key");
        let Some(s) = s_k.arm::<c::Stripe>() else { return Ok(None) };

        /* write buffer race? */
        Ok((s.v().lru_pos() == lru.time).then_some(s))
    }

    fn get_stripe_buckets(&self, batch: &mut Batch<'f>) -> Result<(), BchError> {
        let trans = self.trans();
        let lru = LruPos::range(LruId::Stripes);
        let mut iter = BtreeIter::new(trans, c::btree_id::lru, lru.start().to_bpos(), BtreeIterFlags::empty());

        iter.for_each_max(trans, lru.end().to_bpos(), |t, _, lru_k| {
            let mut s_iter = BtreeIter::new(t, c::btree_id::stripes, c::bpos::default(), BtreeIterFlags::empty());
            let Some(s) = Self::lru_stripe(t, &mut s_iter, lru_k.k.p)? else { return Ok(LoopFlow::Continue) };

            for (ptr, sectors) in data_blocks(s) {
                if sectors == 0 {
                    continue;
                }
                let Ok(Some(ca)) = t.fs().dev_bkey_tryget(s.into(), ptr.dev().into()) else { continue };

                self.try_add(t, batch, ca.ptr_bucket_pos(ptr), u64::MAX)?;

                if batch_full(batch, self.in_flight.len()) {
                    return Ok(LoopFlow::Break(()));
                }
            }
            Ok(LoopFlow::Continue)
        })
    }

    /// Whether this pass should take the stripe lru's buckets: whether the
    /// most fragmented stripe is under half as full as the best bucket we'd
    /// take otherwise.
    ///
    /// Errors from either walk are dropped, and the answer's no: a btree
    /// error here is the filesystem going read-only, which the pass will
    /// find - or a repair underway in the background, which a later pass
    /// sees past. Neither is a reason to stop copygc, and taking ordinary
    /// buckets is always right.
    fn should_do_ec(&self, devs: &KVVec<CopygcDev>) -> bool {
        let trans = self.trans();
        let lru = LruPos::range(LruId::Stripes);
        let mut iter = BtreeIter::new(trans, c::btree_id::lru, lru.start().to_bpos(), BtreeIterFlags::empty());

        let stripe_frag_ratio = iter.for_each_max(trans, lru.end().to_bpos(), |t, _, lru_k| {
            let mut s_iter = BtreeIter::new(t, c::btree_id::stripes, c::bpos::default(), BtreeIterFlags::empty());
            let Some(s) = Self::lru_stripe(t, &mut s_iter, lru_k.k.p)? else { return Ok(LoopFlow::Continue) };

            let v = s.v();
            let nr_data = v.nr_data();
            let blocks_nonempty = data_blocks(s).filter(|(_, n)| *n != 0).count() as u64;

            /* stripe is pending delete */
            if blocks_nonempty == 0 {
                return Ok(LoopFlow::Continue);
            }

            /* This matches the calculation in alloc_lru_idx_fragmentation, so we can
             * directly compare without actually looking up the bucket pointed to by the
             * bucket fragmentation lru:
             */
            Ok(LoopFlow::Break((blocks_nonempty << 31) / u64::from(nr_data)))
        }).unwrap_or(0);

        /*
         * Compare against the best bucket candidate this round can actually
         * evacuate - the emptiest lru head across the devices that need
         * copygc:
         */
        let mut bucket_frag_ratio = 0;
        for d in devs.iter() {
            let lru = fragmentation_lru(d.dev);
            let end = lru.end().to_bpos();
            let mut iter = BtreeIter::new(trans, c::btree_id::lru, lru.start().to_bpos(), BtreeIterFlags::empty());

            if let Ok(Some(p)) = lockrestart_do(trans, |t| Ok(iter.peek_max(t, end)?.map(|k| k.k.p))) {
                let time = LruPos::from_bpos(p).time;
                if bucket_frag_ratio == 0 || time < bucket_frag_ratio {
                    bucket_frag_ratio = time;
                }
            }
        }

        /* Prefer normal bucket copygc */
        stripe_frag_ratio != 0 && stripe_frag_ratio * 2 < bucket_frag_ratio
    }

    // ── A pass ───────────────────────────────────────────────────────────

    /// Drop what's in flight from the oldest, up to the first still moving;
    /// with @flush, wait for each.
    fn wait_done(&mut self, flush: bool) {
        while let Some(b) = self.in_flight.peek_front() {
            if flush {
                self.ctxt.wait_until(|| b.done().then_some(()));
            }
            if !b.done() {
                break;
            }
            drop(self.in_flight.pop_front());
        }
        self.trans().unlock_long();
    }

    fn pick_and_evacuate(&mut self, batch: &mut Batch<'f>, picked: &mut usize, devs: &mut KVVec<CopygcDev>)
        -> Result<(), BchError>
    {
        let fs = self.fs;
        let data_opts = c::data_update_opts {
            type_:        c::bch_data_update_types::BCH_DATA_UPDATE_copygc,
            commit_flags: c::bch_trans_commit_flags::from_bits_retain(c::bch_watermark::BCH_WATERMARK_copygc as u32),
            ..Default::default()
        };

        match self.trans().write_buffer_tryflush() {
            Err(e) if e.matches(c::EROFS) => return Err(e),
            Err(e) => {
                fs_fatal_error!(fs, "{}: from bch2_btree_write_buffer_tryflush()", e.msg());
                return Err(e);
            }
            Ok(()) => {}
        }

        if self.should_do_ec(devs) {
            self.get_stripe_buckets(batch)?;
        } else {
            self.get_buckets(batch, devs)?;
        }

        *picked = batch.len();
        self.in_flight.reserve(batch.len(), GFP_KERNEL)?;
        // popped from the end: in the order picked
        batch.reverse();
        while !batch.is_empty() {
            if Kthread::should_stop() || Kthread::freezing() {
                break;
            }

            let b = batch.pop().expect("not empty");
            let (key, count) = (*b.key(), NonNull::from(&b.b));
            if self.in_flight.push_back(b).is_err() {
                unreachable!("reserved");
            }

            // SAFETY: @count is the InFlight's, pinned, and the FIFO drops
            // it only once the count's back to 0 (wait_done())
            unsafe { self.ctxt.evacuate_bucket(count, key, data_opts) }?;
        }
        Ok(())
    }

    fn copygc_pass(&mut self, devs: &mut KVVec<CopygcDev>) -> (Result<(), BchError>, bool) {
        let fs = self.fs;
        let sectors = |cg: &Self| (cg.ctxt.stats().sectors_seen(), cg.ctxt.stats().sectors_moved());
        let (seen, moved) = sectors(self);

        self.wait_done(false);

        let mut batch = KVVec::new();
        let mut picked = 0;
        let ret = self.pick_and_evacuate(&mut batch, &mut picked, devs);

        let ret = match ret {
            /* no entries in LRU btree found, or got to end: */
            Err(e) if e.matches(c::ENOENT) => Ok(()),
            /* we're being stopped - normal, not something to report: */
            Err(e) if e.matches(bch_errcode::BCH_ERR_kthread_cancelled) => Ok(()),
            r => r,
        };
        let ret = match ret {
            Err(e) if !e.matches(c::EROFS) => bch_err_msg!(fs, Err(e), "from bch2_move_data()"),
            r => r,
        };

        let (seen, moved) = { let (s, m) = sectors(self); (s.wrapping_sub(seen), m.wrapping_sub(moved)) };

        /*
         * An evacuation that processed no extents is not work: a bucket whose
         * backpointers all fail or skip resolution (e.g. transient
         * backpointers to overwritten btree nodes under heavy interior-update
         * churn) evacuates as an instant no-op while is_movable keeps
         * approving it. Counting that as did_work skips the io-clock backoff
         * and copygc respins on the same buckets - observed in the field at
         * ~230k no-op evacuations/sec. sectors_seen counts extents the
         * evacuations actually processed (synchronously, unlike sectors_moved
         * which completes async), so it distinguishes real work from no-op
         * spins:
         */
        let did_work = seen != 0;
        fs.event_inc_trace(events::copygc, |out| {
            write!(out, "buckets {picked} sectors seen {seen} moved {moved}");
        });

        (ret, did_work)
    }

    fn sleep(&mut self, until: u64, kick: u32) {
        self.wait_done(true);
        let me = self.me;
        let _ = self.fs.io_clock[WRITE].wait_until(until, Sleep::INTERRUPTIBLE.freezable(),
            || (me.kicked_since(kick) || Kthread::should_stop()).then_some(()));
    }
}

// ── The thread ───────────────────────────────────────────────────────────

fn thread(fs: &Fs, me: &KthreadSelf<'_>) -> Result<(), BchError> {
    // Before what holds an InFlight: it outlives every one registered in it
    let set = InFlightSet::default();
    let set = &set;
    let mut devs = KVVec::new();
    let mut kick = me.kicks();

    Kthread::set_freezable();

    /*
     * We're started before recovery, so that mount can't fail later on
     * kthread_create(); we run from when the filesystem goes rw, through
     * journal replay: replay can need buckets that only we can free. Moves
     * don't need check_snapshots (bch2_snapshot_is_ancestor() walks parent
     * pointers until it's run), and recovery won't resume stripe updates we
     * start (bch2_logged_ops_note_unfinished()).
     */
    let _ = sleep_until(Sleep::INTERRUPTIBLE.freezable(),
        || (fs.flag(c::bch_fs_flags::BCH_FS_rw) || Kthread::should_stop()).then_some(()));
    if Kthread::should_stop() {
        return Ok(());
    }

    let stats = c::bch_move_stats::default();
    let stats = MoveStats::from_c(&stats);
    stats.reset(c"copygc");
    let ctxt = bch_err_msg!(fs, MovingCtxt::new(fs, stats, writepoint_ptr(&fs.copygc.write_point), false),
                            "allocating copygc's moving context")?;
    let mut cg = Copygc { fs, me, ctxt, set, in_flight: Fifo::new() };
    let clock = &fs.io_clock[WRITE];
    let mut ret = Ok(());

    while ret.is_ok() && !Kthread::should_stop() {
        cg.trans().unlock_long();
        cond_resched();

        if fs.opts().copygc_enabled == 0 {
            cg.wait_done(true);
            let _ = sleep_until(Sleep::INTERRUPTIBLE.freezable(),
                || (fs.opts().copygc_enabled != 0 || Kthread::should_stop()).then_some(()));
        }

        /* Moves in flight stay tracked in @cg across the freeze */
        if Kthread::freezing() {
            Kthread::try_to_freeze();
            continue;
        }

        let last = clock.now.load(Relaxed) as u64;
        let wait = match cg.dev_list(&mut devs) {
            Ok(wait) => wait,
            Err(e) => { ret = Err(e); break; }
        };

        if devs.is_empty() && !me.kicked_since(kick) {
            fs.copygc.wait_at.set(last as i64);
            fs.copygc.wait.set(last.wrapping_add(wait) as i64);
            /*
             * The kick's checked in the sleep's task state: the allocator's
             * kick is either seen there or wakes the sleep - no lost wakeups
             * (the io clock only advances with write throughput, which may
             * be stalled on the kicker).
             */
            cg.sleep(last.wrapping_add(wait), kick);
            continue;
        }

        kick = me.kicks();
        fs.copygc.wait.set(0);

        fs.copygc.running.set(true);
        let (r, did_work) = cg.copygc_pass(&mut devs);
        fs.copygc.running.set(false);
        fs.copygc.run_count.set(fs.copygc.run_count.get().wrapping_add(1));
        fs.copygc.run_done.wake();
        ret = r;

        if wait == 0 && !did_work {
            let capacity = match fs.min_rw_member_capacity() {
                u64::MAX => 128 * 2048,
                c => c,
            };
            cg.sleep(last + (capacity >> 6), kick);
        }
    }

    cg.wait_done(true);
    ret
}

impl Fs {
    pub fn copygc_wakeup(&self) {
        if let Some(t) = &*self.copygc.thread.lock() {
            t.kick();
        }
    }

    /// And its workqueue, for the btree updates of its moves.
    pub fn copygc_start(&self) -> Result<(), BchError> {
        if self.opts().nochanges != 0 {
            return Ok(());
        }

        if !self.copygc.wq.is_some() {
            let wq = Workqueue::new(format_args!("bcachefs_copygc"),
                                    WqFlags::HIGHPRI | WqFlags::FREEZABLE | WqFlags::MEM_RECLAIM | WqFlags::UNBOUND, 1)
                .map_err(|_| self.err(bch_errcode::BCH_ERR_ENOMEM_fs_other_alloc))?;
            // SAFETY: none to drop; start and exit are serialized by the
            // caller, and nothing queues on it until it's made
            unsafe { core::ptr::write(&raw mut (*self.as_ptr()).copygc.wq, wq) };
        }

        let cg = &self.copygc;
        if cg.thread.lock().is_some() {
            return Ok(());
        }

        // SAFETY: the thread's stopped before the filesystem goes -
        // copygc_stop(), on the way to read-only
        let fs: &'static Fs = unsafe { &*self.as_ptr() };
        let t = Kthread::create(format_args!("bch-copygc/{}", self.name().to_str().unwrap_or("?")),
                                move |me| { let _ = thread(fs, me); });
        let t = bch_err_msg!(self, t, "creating copygc thread")?;

        cg.thread.lock().insert(t).wake();
        Ok(())
    }

    /// Outside the lock, which kicks take: kthread_stop() waits.
    pub fn copygc_stop(&self) {
        let t = self.copygc.thread.lock().take();
        drop(t);
    }

    /// Until a copygc run ends after @run_count, or this kthread's told to
    /// stop: the run count then. Freezable, as copygc freezes in place; idle,
    /// as a run can take longer than the hung task timeout.
    pub fn copygc_wait_run(&self, run_count: u32) -> u32 {
        let cg = &self.copygc;
        let Ok(()) = cg.run_done.wait_until(Sleep::IDLE.freezable(),
                                            || (cg.run_count.get() != run_count || Kthread::should_stop()).then_some(()));
        cg.run_count.get()
    }

    #[cold]
    pub fn copygc_wait_to_text(&self, out: &mut Printbuf) {
        let cg = &self.copygc;
        let now = self.io_clock[WRITE].now.load(Relaxed);

        out.tabstop_push(32);
        write!(out, "running:\t{}\n", u32::from(cg.running.get()));
        write!(out, "run count:\t{}\n", cg.run_count.get());
        write!(out, "copygc_wait:\t{}\n", cg.wait.get() as u64);
        write!(out, "copygc_wait_at:\t{}\n", cg.wait_at.get() as u64);

        write!(out, "Currently waiting for:\t");
        out.human_readable_u64((cg.wait.get().wrapping_sub(now).max(0) as u64) << 9);
        out.newline();

        write!(out, "Currently waiting since:\t");
        out.human_readable_u64((now.wrapping_sub(cg.wait_at.get()).max(0) as u64) << 9);
        out.newline();

        out.make_room_gfp(4096, c::GFP_KERNEL);

        {
            let _mark = self.mark_lock().read();
            let rcu = read_lock();
            let mut out = out.atomic();
            write!(out, "Currently calculated wait:\n");
            for ca in self.rw_devs_rcu(&rcu) {
                write!(out, "  ");
                out.write_bytes(ca.name().to_bytes());
                write!(out, ":\t");
                out.human_readable_s64(dev_wait_amount(ca).0);
                out.newline();
            }
        }

        // a ref of our own, to print it outside the lock: that allocates
        let task = cg.thread.lock().as_ref().map(|t| {
            // SAFETY: the Kthread's task, live while it's in the lock
            unsafe { c::get_task_struct(t.as_ptr()) };
            t.as_ptr()
        });
        if let Some(task) = task {
            // SAFETY: our ref, put after
            unsafe {
                c::bch2_prt_task_backtrace(out.as_raw(), task, 0, c::GFP_KERNEL);
                c::put_task_struct(task);
            }
        }
    }
}

// ── For C ────────────────────────────────────────────────────────────────

#[c_export]
pub fn bch2_copygc_wakeup(c: &Fs) {
    c.copygc_wakeup()
}

#[c_export]
pub fn bch2_copygc_start(c: &Fs) -> c_int {
    ret_to_c(c.copygc_start())
}

#[c_export]
pub fn bch2_copygc_stop(c: &Fs) {
    c.copygc_stop()
}

#[c_export]
pub fn bch2_fs_copygc_exit(c: &Fs) {
    // SAFETY: the thread's stopped, and what queued on it with it: the
    // filesystem's going
    drop(unsafe { core::ptr::replace(&raw mut (*c.as_ptr()).copygc.wq, Workqueue::default()) });
}

/// mark_lock held.
#[c_export]
pub fn bch2_copygc_can_make_progress(ca: &c::bch_dev) -> bool {
    can_make_progress(ca)
}

/// mark_lock held; @low_on_space, if not null, set.
///
/// # Safety
/// @low_on_space is null, or a bool to write.
#[c_export]
pub unsafe fn bch2_copygc_dev_wait_amount(ca: &c::bch_dev, low_on_space: *mut bool) -> i64 {
    let (wait, low) = dev_wait_amount(ca);
    if !low_on_space.is_null() {
        // SAFETY: the caller's
        unsafe { *low_on_space = low };
    }
    wait
}

#[c_export]
pub fn bch2_copygc_wait_to_text(out: &mut Printbuf, c: &Fs) {
    c.copygc_wait_to_text(out)
}

/// Until copygc isn't running, or this kthread's told to stop: for a moving
/// context that waits on copygc, bch2_move_ratelimit()'s. Freezable - copygc
/// freezes in place, mid run - and interruptible, as wait_event_freezable().
#[c_export]
pub fn bch2_copygc_wait_not_running(c: &Fs) {
    let cg = &c.copygc;
    let _ = cg.run_done.wait_until(Sleep::INTERRUPTIBLE.freezable(),
                                   || (!cg.running.get() || Kthread::should_stop()).then_some(()));
}
