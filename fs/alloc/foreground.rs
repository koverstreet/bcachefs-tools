// SPDX-License-Identifier: GPL-2.0

//! The foreground allocator's core: open buckets - a pin on one, and the
//! hash that says whether a bucket is open - and allocation requests, their
//! trace, and waiting on the allocator. Which devices and buckets a request
//! gets are dev_select.rs's and bucket_alloc.rs's, its set of buckets
//! bucket_set.rs's; write points and the sector allocator are
//! write_point.rs's and sector_alloc.rs's.
//!
//! An open bucket is a slot in the allocator's open_buckets[], named by its
//! index: a bucket handed out to be written, kept open by its pins - ->pin, a
//! refcount. The last put closes it: back on the freelist, or for a block of
//! a stripe being made, its ->ec's io ref dropped. Pins move by index - an
//! open_buckets list (a write point's, an alloc_request's) - and an
//! OpenBucket is one Rust holds, a RefGuard on the bucket's pins: into_idx()
//! hands it to such a list, from_idx() takes one back. While it's pinned, its
//! identity - OpenBucketId: dev, generation, bucket - doesn't change;
//! sectors_free is its writer's.
//!
//! A block of a stripe being made that a writer has claimed is open past its
//! last put, for the stripe: an EcOpenBucket, the other owning handle, which
//! closes it. Both deref to an OpenBucketRef, the bucket borrowed - which is
//! also what one C holds a pin on is, borrowed from C: from_c(), from_idx().
//!
//! What an open bucket is for: buckets partly written, for sub-bucket
//! allocations - the sector allocator's write points and partial list - and
//! a reference GC can find to buckets an allocation has but no key yet
//! points to. So a writer puts its pin only after the index update that
//! makes its allocation reachable.
//!
//! The hash is OPEN_BUCKETS_COUNT slots, each the head of a chain through
//! the open buckets' ->hash, 0 ending it, a bucket's slot hash_64() of its
//! (dev, bucket). C finds the slot here too - bch2_open_bucket_hash(), for
//! open_bucket_hashslot() - so the two can't disagree. It's written under
//! freelist_lock and read without it - by C's bch2_bucket_is_open() too -
//! each slot and link once, volatile, and each bucket's identity whole. A
//! bucket opened or closed while we look may be seen or missed: a caller for
//! whom that matters looks again under the lock, bucket_is_open_safe().
//!
//! A request that finds nothing free waits - on freelist_wait for buckets,
//! open_buckets_wait for open buckets - and retries when a wake may be for
//! it: its trace records each attempt, with the device's wake counter as it
//! was, and a wake is for it if one it tried has moved since.

use crate::alloc::discard::fast_discard_bucket_add;
use crate::alloc::disk_groups::Target;
use crate::btree::iter::{BtreeTrans, TransAttempt};
use crate::c;
use crate::c::bch_errcode;
use crate::data::copygc::can_make_progress;
use crate::errcode::{err_ptr, ret_to_c, ret_to_result_void, BchError};
use crate::fs::Fs;
use crate::locking::Guard;
use crate::opts::data_type_str;
use crate::percpu_field;
use crate::sb::counters::events;
use crate::sb::members::{data_types_to_text, member_state_str};
use crate::types::RustOnly;
use crate::util::alloc::flags::GFP_KERNEL;
use crate::util::alloc::init_at;
use crate::util::closure::{closure_has_waiters, closure_wait, closure_wake_up, StackClosure};
use crate::util::time_stats::TimeStats;
use crate::util::ffi::Opaque;
use crate::util::hash::{hash_64, Fold};
use crate::util::inline_vec::InlineVec;
use crate::util::kernel::{jiffies, time_after, unlikely, HZ};
use crate::util::log::{print_str, LogLevel};
use crate::util::place::Placed;
use crate::util::projections::Volatile;
use crate::util::rcu::read_lock;
use crate::util::refcount::{RefGuard, Refcount};
use crate::util::{Printbuf, ToText};
use core::ffi::{c_int, c_long, c_uint, c_ulong};
use core::convert::Infallible;
use core::ops::Deref;
use core::ptr::NonNull;
use core::sync::atomic::Ordering::{Relaxed, SeqCst};
use cstruct_macros::c_export;

const _: () = assert!(c::OPEN_BUCKETS_COUNT.is_power_of_two());

// ── Open buckets ─────────────────────────────────────────────────────────

/// An open bucket, borrowed while it's held open - by a pin, or its ->ec.
/// Every field written meanwhile - by C, or another's Rust - is an atomic, a
/// Volatile, or its lock; what's read when is open_bucket's (alloc/types.rs).
#[repr(transparent)]
pub struct OpenBucketRef(c::open_bucket);

impl OpenBucketRef {
    /// # Safety
    /// @ob is open, and held open for 'a.
    pub unsafe fn from_c<'a>(ob: *const c::open_bucket) -> &'a Self {
        // SAFETY: a transparent open_bucket; the caller's
        unsafe { &*ob.cast() }
    }

    /// # Safety
    /// Open bucket @idx is open, and held open for 'a.
    pub unsafe fn from_idx<'a>(fs: &'a Fs, idx: c::OpenBucketIdx) -> &'a Self {
        // SAFETY: the caller's
        unsafe { Self::from_c(&fs.allocator.open_buckets[idx.get()]) }
    }

    /// Its filesystem: its allocator's.
    fn fs(&self) -> &Fs {
        self.0.container().container()
    }

    /// Its device, which it keeps: device removal stops its open buckets
    /// before the device goes.
    pub fn ca(&self) -> &c::bch_dev {
        let p = self.fs().devs[usize::from(self.id().dev())].as_ptr();
        // SAFETY: an open bucket's device outlives it, as above
        unsafe { p.as_ref() }.expect("an open bucket's device")
    }

    /// Where its free space starts.
    pub fn ptr(&self) -> c::bch_extent_ptr {
        let id = self.id();
        let ca = self.ca();

        c::bch_extent_ptr::new()
            .with_generation(id.generation())
            .with_dev(id.dev())
            .with_offset(ca.bucket_to_sector(id.bucket()) + u64::from(ca.mi.get().bucket_size) -
                         u64::from(self.sectors_free.get()))
    }
}

impl Deref for OpenBucketRef {
    type Target = c::open_bucket;

    fn deref(&self) -> &c::open_bucket {
        &self.0
    }
}

// SAFETY: its fields atomics and Volatiles but its lock, C's spinlock, which
// is any thread's to take
unsafe impl Sync for OpenBucketRef {}

// SAFETY: an OpenBucketRef is had only while it's held open - by a pin, or
// its ->ec - so inc() is from a pin that's held; the last put closes it
unsafe impl Refcount for OpenBucketRef {
    fn inc(&self) {
        self.pin.fetch_add(1, Relaxed);
    }

    unsafe fn dec(&self) {
        // fully ordered, as atomic_dec_and_test()
        if self.pin.fetch_sub(1, SeqCst) == 1 {
            self.fs().open_bucket_close(self);
        }
    }
}

/// A pin on an open bucket: it's open while one's held.
pub type OpenBucket = RefGuard<OpenBucketRef>;

impl RefGuard<OpenBucketRef> {
    /// # Safety
    /// The caller holds a pin on open bucket @idx, and hands it over.
    pub unsafe fn from_idx(fs: &Fs, idx: c::OpenBucketIdx) -> Self {
        // SAFETY: the caller's
        unsafe { RefGuard::adopt(OpenBucketRef::from_idx(fs, idx)) }
    }

    pub fn into_idx(self) -> c::OpenBucketIdx {
        let idx = self.idx();
        self.leak();
        idx
    }

    /// Claimed by the writer of block @ec_idx of stripe @ec: its pin, given
    /// back, the writer's - the bucket, for it - and its ->ec what keeps it
    /// open for the stripe.
    pub fn claim(self, ec: *mut c::ec_stripe_new, ec_idx: u8) -> (EcOpenBucket, Self) {
        // the pin's holder's to set, before the bucket's handed on
        self.ec_idx.set(ec_idx);
        self.ec.store(ec, Relaxed);
        (EcOpenBucket { ob: NonNull::from(&**self) }, self)
    }
}

/// A block of a stripe being made, claimed by a writer: open for the stripe,
/// past its last pin, and closed when it's dropped - once the stripe's done
/// with it.
pub struct EcOpenBucket {
    ob: NonNull<c::open_bucket>,
}

// SAFETY: as OpenBucket's: closing it takes the allocator's locks, and what's
// read while it's open is fixed
unsafe impl Send for EcOpenBucket {}
unsafe impl Sync for EcOpenBucket {}

impl Deref for EcOpenBucket {
    type Target = OpenBucketRef;

    fn deref(&self) -> &OpenBucketRef {
        // SAFETY: it holds it open while it's borrowed
        unsafe { OpenBucketRef::from_c(self.ob.as_ptr()) }
    }
}

impl Drop for EcOpenBucket {
    fn drop(&mut self) {
        let ob: &c::open_bucket = self;
        assert_eq!(ob.pin.load(SeqCst), 0,
                   "closing open bucket {}, claimed by a writer still holding its pin", self.idx().get());
        // open only for its stripe, which this ends
        ob.ec.store(core::ptr::null_mut(), Relaxed);
        self.fs().open_bucket_close(self);
    }
}

impl c::open_buckets {
    /// Its buckets, borrowed: the list's pins keep them open while it is.
    pub fn iter<'a>(&'a self, fs: &'a Fs) -> impl Iterator<Item = &'a OpenBucketRef> + 'a {
        self.v[..usize::from(self.nr)].iter().map(move |&i| {
            // SAFETY: the list's pin, held while the list's borrowed
            unsafe { OpenBucketRef::from_idx(fs, i.expect("a bucket of the list")) }
        })
    }

    /// @ob added, the list holding its pin.
    pub fn push(&mut self, ob: OpenBucket) {
        let nr = usize::from(self.nr);
        assert!(nr < self.v.len(), "open bucket list full");
        self.v[nr] = Some(ob.into_idx());
        self.nr += 1;
    }

    /// Its @i'th taken out, the rest moving down: its pin, which the list no
    /// longer has.
    pub fn remove(&mut self, fs: &Fs, i: usize) -> OpenBucket {
        let nr = usize::from(self.nr);
        let idx = self.v[..nr][i].expect("a bucket of the list");
        self.v.copy_within(i + 1..nr, i);
        self.nr -= 1;
        // SAFETY: the list's pin, which it no longer has
        unsafe { OpenBucket::from_idx(fs, idx) }
    }

    /// Its pins, taken one by one - a list holds one on each of its buckets -
    /// leaving it empty: the rest put when the iterator's dropped. Not
    /// collected: a list of them is a copy, on the stack, of what the list
    /// already has.
    pub fn drain<'f>(&mut self, fs: &'f Fs) -> OpenBucketsDrain<'f, '_> {
        let nr = core::mem::take(&mut self.nr) as usize;
        OpenBucketsDrain { fs, idxs: self.v[..nr].iter() }
    }
}

/// open_buckets::drain()'s: pins the list no longer has.
pub struct OpenBucketsDrain<'f, 'l> {
    fs:   &'f Fs,
    idxs: core::slice::Iter<'l, Option<c::OpenBucketIdx>>,
}

impl<'f> Iterator for OpenBucketsDrain<'f, '_> {
    type Item = OpenBucket;

    fn next(&mut self) -> Option<OpenBucket> {
        let idx = self.idxs.next()?.expect("a bucket of the list");
        // SAFETY: the list's pin, which it no longer has, handed out once
        Some(unsafe { OpenBucket::from_idx(self.fs, idx) })
    }
}

impl Drop for OpenBucketsDrain<'_, '_> {
    fn drop(&mut self) {
        self.for_each(drop);
    }
}

/// freelist_lock, held.
pub(crate) type FreelistGuard<'a> = Guard<'a, c::spinlock_t, c::OpenBucketsFree, c::OpenBucketsFreeNr>;

/// An open bucket's lock, held.
pub(crate) type ObGuard<'a> = Guard<'a, c::spinlock_t, (), c::OpenBucketIdentity>;

impl c::open_bucket {
    pub const ON_PARTIAL_LIST: u8  = 1 << 0;
    pub const DO_DISCARDS_FAST: u8 = 1 << 1;

    /// Which bucket it is, and whether it's open: read whole.
    pub fn id(&self) -> c::OpenBucketId {
        self.lock.id.get()
    }

    pub fn flag(&self, f: u8) -> bool {
        self.flags.load(Relaxed) & f != 0
    }

    /// @f set if @v, else cleared: whether it was set.
    pub fn set_flag(&self, f: u8, v: bool) -> bool {
        let old = if v { self.flags.fetch_or(f, Relaxed) } else { self.flags.fetch_and(!f, Relaxed) };
        old & f != 0
    }

    /// Its index, which the hash and the lists link it by.
    pub fn idx(&self) -> c::OpenBucketIdx {
        c::OpenBucketIdx::new(self.idx.get()).expect("an open bucket, not the sentinel")
    }

    /// The bucket, if it's open: valid, under its lock, which the close
    /// clears.
    pub fn open_ref<'a>(&'a self, _lock: &'a ObGuard<'_>) -> Option<&'a OpenBucketRef> {
        // SAFETY: valid under its lock: open till the lock's dropped
        self.id().valid().then(|| unsafe { OpenBucketRef::from_c(self) })
    }
}

// ── The hash ─────────────────────────────────────────────────────────────

impl c::bch_fs_allocator {
    fn hash_slot(&self, dev: u32, bucket: u64) -> &Volatile<Option<c::OpenBucketIdx>> {
        &self.open_buckets_hash[bch2_open_bucket_hash(dev, bucket) as usize]
    }

    /// @ob into the hash, at its chain's head.
    fn hash_add(&self, _g: &FreelistGuard<'_>, ob: &c::open_bucket) {
        let id = ob.id();
        let slot = self.hash_slot(id.dev().into(), id.bucket());
        ob.hash.set(slot.get());
        slot.set(Some(ob.idx()));
    }

    /// @ob out of the hash: its chain walked to the link to it.
    fn hash_remove(&self, _g: &FreelistGuard<'_>, ob: &c::open_bucket) {
        let me = Some(ob.idx());
        let id = ob.id();
        let mut slot = self.hash_slot(id.dev().into(), id.bucket());
        while slot.get() != me {
            slot = &self.open_buckets[slot.get().expect("an open bucket in its hash chain").get()].hash;
        }
        slot.set(ob.hash.get());
        ob.hash.set(None);
    }
}

impl Fs {
    /// The open bucket @dev's @bucket is, if it's open - found racing the
    /// allocator, so to be relied on only under freelist_lock, which opening
    /// and closing hold.
    pub fn open_bucket(&self, dev: u32, bucket: u64) -> Option<&c::open_bucket> {
        let a = &self.allocator;
        let mut i = a.hash_slot(dev, bucket).get();
        while let Some(idx) = i {
            let ob = &a.open_buckets[idx.get()];
            let id = ob.id();
            if u32::from(id.dev()) == dev && id.bucket() == bucket {
                return Some(ob);
            }
            i = ob.hash.get();
        }
        None
    }

    /// Whether @dev's @bucket is open, as open_bucket() finds it.
    pub fn bucket_is_open(&self, dev: u32, bucket: u64) -> bool {
        self.open_bucket(dev, bucket).is_some()
    }

    /// @dev's open @bucket to be discarded as soon as it's freed - false if
    /// it isn't open. Rechecked under the bucket's lock, which the close
    /// takes: found without it, it may since have been reopened as another.
    pub fn bucket_set_discard_fast(&self, dev: u32, bucket: u64) -> bool {
        let Some(ob) = self.open_bucket(dev, bucket) else { return false };
        let _l = ob.lock.lock();
        let id = ob.id();
        if u32::from(id.dev()) != dev || id.bucket() != bucket {
            return false;
        }
        ob.set_flag(c::open_bucket::DO_DISCARDS_FAST, true);
        true
    }

    /// bucket_is_open(), and if not, again under freelist_lock, which opening
    /// a bucket holds.
    pub fn bucket_is_open_safe(&self, dev: u32, bucket: u64) -> bool {
        self.bucket_is_open(dev, bucket) || {
            let _freelist = self.allocator.freelist_lock.lock();
            self.bucket_is_open(dev, bucket)
        }
    }
}

// ── Opening and closing ──────────────────────────────────────────────────

/// The open buckets kept back from allocations at @watermark: more, the lower
/// it is, so what frees open buckets can still get them.
fn open_buckets_reserved(watermark: c::bch_watermark) -> u32 {
    use c::bch_watermark::*;
    match watermark {
        BCH_WATERMARK_interior_updates                   => 0,
        BCH_WATERMARK_reclaim                            => c::OPEN_BUCKETS_COUNT / 6,
        BCH_WATERMARK_btree | BCH_WATERMARK_btree_copygc => c::OPEN_BUCKETS_COUNT / 4,
        BCH_WATERMARK_copygc                             => c::OPEN_BUCKETS_COUNT / 3,
        _                                                => c::OPEN_BUCKETS_COUNT / 2,
    }
}

impl c::bch_fs_allocator {
    /// The partial list's buckets, its pins held while @g is.
    pub(crate) fn partial<'a>(&'a self, g: &'a FreelistGuard<'_>)
        -> impl DoubleEndedIterator<Item = &'a OpenBucketRef> + ExactSizeIterator + 'a
    {
        let n = usize::from(self.freelist_lock.open_buckets_partial_nr.get());
        g.open_buckets_partial[..n].iter().map(move |&i| {
            // SAFETY: the partial list's pin, which it holds while the lock is
            unsafe { OpenBucketRef::from_c(&self.open_buckets[i.expect("a bucket of the partial list").get()]) }
        })
    }
}

impl Fs {
    /// The lowest watermark the open bucket pool can still serve - 0 if it
    /// isn't starved: one short of the top, to leave interior updates a way.
    pub(crate) fn open_buckets_starved_watermark(&self) -> u32 {
        let nr_free = u32::from(self.allocator.freelist_lock.open_buckets_nr_free.get());
        (0..c::BCH_WATERMARK_NR - 1).find(|&w| nr_free >= open_buckets_reserved(c::bch_watermark::from_bits(w)))
            .unwrap_or(c::BCH_WATERMARK_NR - 1)
    }

    /// Bucket @bucket of @ca, of generation @generation, opened: None if it's
    /// open already. Err with the pool down to @req's watermark's reserve -
    /// blocked, with @req's closure on open_buckets_wait.
    pub(crate) fn try_alloc_bucket_locked(&self, ca: &c::bch_dev, req: &mut c::alloc_request, bucket: u64,
                                          generation: u8) -> Result<Option<OpenBucket>, BchError>
    {
        let a = &self.allocator;
        let times = |t: c::bch_time_stats| &self.times[t as usize];
        let dev = ca.dev_idx;

        let mut g = a.freelist_lock.lock();

        if unlikely(u32::from(a.freelist_lock.open_buckets_nr_free.get()) <= open_buckets_reserved(req.watermark)) {
            times(c::bch_time_stats::BCH_TIME_blocked_allocate_open_bucket).track_event_change(true);
            return Err(self.err(match req.closure_wait(&a.open_buckets_wait) {
                true  => bch_errcode::BCH_ERR_open_bucket_alloc_blocked,
                false => bch_errcode::BCH_ERR_open_buckets_empty,
            }));
        }

        // Recheck under lock:
        if self.bucket_is_open(dev.into(), bucket) {
            req.counters.skipped_open += 1;
            return Ok(None);
        }

        // a slot off the freelist, with its first pin
        let (free, nr) = g.parts();
        let ob = &a.open_buckets[free.open_buckets_freelist.expect("a free open bucket").get()];
        free.open_buckets_freelist = ob.freelist.get();
        *nr.open_buckets_nr_free.place() -= 1;
        ob.pin.store(1, Relaxed);
        ob.data_type.set(0);
        {
            let mut l = ob.lock.lock();
            ob.sectors_free.set(ca.mi.get().bucket_size.into());
            ob.free_uncounted.set(false);
            l.published().id.set(c::OpenBucketId::new()
                .with_valid(true)
                .with_dev(dev)
                .with_generation(generation)
                .with_bucket(bucket));
        }

        ca.nr_open_buckets.set(ca.nr_open_buckets.get().wrapping_add(1));
        a.hash_add(&g, ob);

        times(c::bch_time_stats::BCH_TIME_blocked_allocate_open_bucket).track_event_change(false);
        times(c::bch_time_stats::BCH_TIME_blocked_allocate).track_event_change(false);
        // SAFETY: the pin just taken, ours
        Ok(Some(unsafe { OpenBucket::from_idx(self, ob.idx()) }))
    }

    /// Open bucket @ob closed, its last pin put: back on the freelist - or
    /// for a block of a stripe being made, its io ref on the stripe put.
    fn open_bucket_close(&self, ob: &OpenBucketRef) {
        if let Some(s) = ob.stripe() {
            // SAFETY: the io ref its ->ec has, which closing it gives up
            unsafe { s.put_io() };
            return;
        }

        let o: &c::open_bucket = ob;
        {
            let mut l = o.lock.lock();
            l.published().id.set(o.id().with_valid(false));
            o.data_type.set(0);
        }

        let ca = ob.ca();
        if o.set_flag(c::open_bucket::DO_DISCARDS_FAST, false) {
            fast_discard_bucket_add(ca, ob.id().bucket());
        }

        {
            let a = &self.allocator;
            let mut g = a.freelist_lock.lock();
            a.hash_remove(&g, o);

            let (free, nr) = g.parts();
            o.freelist.set(free.open_buckets_freelist);
            free.open_buckets_freelist = Some(o.idx());
            *nr.open_buckets_nr_free.place() += 1;

            if !o.free_uncounted.get() {
                ca.nr_open_buckets.set(ca.nr_open_buckets.get().wrapping_sub(1));
            }
        }

        // may lift the journal's throttle, which nothing else would notice;
        // freelist_lock dropped, so j->lock doesn't nest under it
        self.journal.set_open_buckets_watermark(self.open_buckets_starved_watermark());

        closure_wake_up(&self.allocator.open_buckets_wait);
    }

    /// Open bucket @ob, which its holder won't write to after all, onto the
    /// partial list - its pin with it - for the next allocation to finish.
    pub fn open_bucket_free_unused(&self, ob: OpenBucket) {
        let a = &self.allocator;
        let dev = ob.id().dev().into();
        {
            let mut g = a.freelist_lock.lock();
            let (free, nr) = g.parts();
            let n = usize::from(nr.open_buckets_partial_nr.get());
            assert!(n < free.open_buckets_partial.len(), "the partial list overflowing");

            let ca = ob.ca();
            ca.nr_partial_buckets.set(ca.nr_partial_buckets.get() + 1);
            ob.set_flag(c::open_bucket::ON_PARTIAL_LIST, true);
            free.open_buckets_partial[n] = Some(ob.into_idx());
            *nr.open_buckets_partial_nr.place() += 1;
        }

        closure_wake_up(&a.open_buckets_wait);
        if let Some(ca) = self.dev_rcu(dev, &read_lock()) {
            ca.alloc_wake();
        }
    }

    /// The partial list's @i'th bucket taken off it, the list's pin with it.
    pub(crate) fn open_buckets_partial_take(&self, g: &mut FreelistGuard<'_>, i: usize) -> OpenBucket {
        let (free, nr) = g.parts();
        let n = usize::from(nr.open_buckets_partial_nr.get());
        let idx = free.open_buckets_partial[..n][i].expect("a bucket of the partial list");
        free.open_buckets_partial.copy_within(i + 1..n, i);
        *nr.open_buckets_partial_nr.place() -= 1;

        // SAFETY: the partial list's pin, which it no longer has
        let ob = unsafe { OpenBucket::from_idx(self, idx) };
        ob.set_flag(c::open_bucket::ON_PARTIAL_LIST, false);
        let ca = ob.ca();
        ca.nr_partial_buckets.set(ca.nr_partial_buckets.get() - 1);
        ob
    }

    /// Whether @ob goes, stopping @ca - or with @ec, erasure coding - or the
    /// whole filesystem, with neither.
    pub(crate) fn open_bucket_should_drop(&self, ob: &OpenBucketRef, ca: Option<&c::bch_dev>, ec: bool) -> bool {
        match ca {
            _ if ec  => ob.stripe().is_some(),
            Some(ca) => ob.id().dev() == ca.dev_idx ||
                        ob.stripe().is_some_and(|s| s.has_dev(ca.dev_idx.into())),
            None     => true,
        }
    }

    /// The partial list's buckets that go - see open_bucket_should_drop() -
    /// taken off it and put.
    pub(crate) fn open_buckets_partial_stop(&self, ca: Option<&c::bch_dev>, ec: bool) {
        let a = &self.allocator;
        let mut i = 0;
        loop {
            // put without the lock, as the close takes it
            let _put = {
                let mut g = a.freelist_lock.lock();
                let Some(j) = a.partial(&g).skip(i).position(|ob| self.open_bucket_should_drop(ob, ca, ec)) else { break };
                i += j;

                // the last into its place: the list's order doesn't matter
                let (free, nr) = g.parts();
                let n = usize::from(nr.open_buckets_partial_nr.get()) - 1;
                free.open_buckets_partial.swap(i, n);
                self.open_buckets_partial_take(&mut g, n)
            };
        }
    }
}

// ── Waking allocations ───────────────────────────────────────────────────

impl c::bch_dev {
    /// Wake what's waiting on the allocator for a bucket on it: its counter
    /// bumped tells a waiter that tried it the wake is for it.
    pub fn alloc_wake(&self) {
        self.alloc_wake_counter.fetch_add(1, Relaxed);
        closure_wake_up(&self.fs().allocator.freelist_wait);
    }
}

impl Fs {
    /// Wake everything waiting on the allocator, for a full retry: a device
    /// in no waiter's trace may have changed - a new one.
    pub fn alloc_wake_all(&self) {
        self.allocator.wake_all_counter.fetch_add(1, Relaxed);
        closure_wake_up(&self.allocator.freelist_wait);
    }

    /// Wake the freelist's waiters with no counter bumped: the only way off a
    /// waitlist for one giving up. The rest wait again.
    pub fn alloc_waiters_unpark(&self) {
        closure_wake_up(&self.allocator.freelist_wait);
    }
}

// ── Requests ─────────────────────────────────────────────────────────────

/// An allocation request, in transaction memory - an attempt's - its trace
/// emptied when it's dropped: alloc_request_put().
pub struct AllocRequest<'a>(&'a mut c::alloc_request);

impl<'a, 't> TransAttempt<'a, 't> {
    /// A request for @nr_replicas copies - or erasure coded, if
    /// @erasure_code and @ec_replicas is 2 or more - in the attempt's memory,
    /// its trace emptied when it's dropped.
    #[allow(clippy::too_many_arguments)]
    pub fn alloc_request(&self, target: u32, erasure_code: bool, devs_have: *mut c::bch_devs_list,
                         nr_replicas: u32, ec_replicas: u32, watermark: c::bch_watermark,
                         flags: c::bch_write_flags, cl: *mut c::closure) -> Result<AllocRequest<'a>, BchError> {
        let req = c::alloc_request::new(self.fs(), target, erasure_code, devs_have, nr_replicas, ec_replicas,
                                        watermark, flags, cl);
        Ok(AllocRequest(self.alloc_init(req)?))
    }

    /// After @req's allocation failed with @err: asleep until there's reason
    /// to retry - if @cl went on a waitlist for it - then Ok, for the caller
    /// to retry, unless the filesystem's gone emergency read-only, which
    /// won't allocate again.
    pub fn wait_on_allocator(&self, req: &mut c::alloc_request, err: BchError, cl: &StackClosure)
        -> Result<(), BchError>
    {
        if cl.nr_remaining() > 1 {
            self.trans().sleep_on_allocator(req, err, cl.as_opaque());
        }
        let fs = self.fs();
        if fs.flag(c::bch_fs_flags::BCH_FS_emergency_ro) {
            return fs.throw(crate::c::bch_errcode::BCH_ERR_emergency_ro);
        }
        Ok(())
    }
}

impl c::alloc_request {
    /// alloc_request_get()'s: zeroed, but for what it's given, and its trace.
    #[allow(clippy::too_many_arguments)]
    fn new(fs: &Fs, target: u32, erasure_code: bool, devs_have: *mut c::bch_devs_list, nr_replicas: u32,
           ec_replicas: u32, watermark: c::bch_watermark, flags: c::bch_write_flags, cl: *mut c::closure)
        -> impl ::pin_init::Init<Self, Infallible> + '_
    {
        // SAFETY: zeroed - each of its fields but the trace a C type, valid
        // zeroed; the trace's storage, bytes - then each written in place,
        // through the pointer: no reference to it till the trace is made
        unsafe {
            ::pin_init::init_from_closure(move |slot: *mut Self| {
                slot.write_bytes(0, 1);
                (&raw mut (*slot).cl).write(cl);
                (&raw mut (*slot).wake_all_counter_snapshot).write(fs.allocator.wake_all_counter.load(Relaxed) as u32);
                (&raw mut (*slot).nr_replicas).write(nr_replicas as u8);
                (&raw mut (*slot).ec_replicas).write(ec_replicas as u8);
                (&raw mut (*slot).bits).write(c::alloc_request_bits::new().with_ec(erasure_code && ec_replicas >= 2));
                (&raw mut (*slot).target).write(target);
                (&raw mut (*slot).watermark).write(watermark);
                (&raw mut (*slot).flags).write(flags);
                (&raw mut (*slot).devs_have).write(devs_have);
                init_at(RustOnly::new(InlineVec::new()), &raw mut (*slot).trace)
            })
        }
    }

    /// Its trace emptied - its storage freed, if it spilled to the heap - for
    /// a retry, and by alloc_request_put().
    pub(crate) fn trace_reset(&mut self) {
        drop(core::mem::take(&mut *self.trace));
    }

    /// The devices already holding a copy, if its caller said.
    pub fn devs_have(&self) -> Option<&c::bch_devs_list> {
        // SAFETY: null, or the caller's list, which outlives the request
        unsafe { self.devs_have.as_ref() }
    }

    /// An attempt recorded in its trace - on @dev, BCH_SB_MEMBER_INVALID for
    /// none, or U8_MAX for the open bucket pool - for the allocator stuck
    /// dump, @err its error if it failed. @wake_counter_snapshot is the
    /// device's wake counter as it was before the attempt waited, or a wake
    /// racing the wait is lost. A trace that can't grow is marked.
    pub fn trace_add(&mut self, dev: u8, err: Option<BchError>, wake_counter_snapshot: u32, free_buckets: u64,
                     copygc_can_make_progress: bool) {
        let b = self.bits;
        let e = c::alloc_trace_entry {
            dev,
            bits: c::alloc_trace_entry_bits::new()
                .with_new_stripe_alloc(b.new_stripe_alloc())
                .with_will_retry_all_devices(b.will_retry_all_devices())
                .with_will_retry_target_devices(b.will_retry_target_devices())
                .with_will_retry_set_devices(b.will_retry_set_devices())
                .with_copygc_can_make_progress(copygc_can_make_progress)
                .with_have_cl(!self.cl.is_null()),
            err: err.map_or(0, |e| ret_to_c(Err(e)) as i16),
            wake_counter_snapshot,
            free_buckets,
        };
        if self.trace.push(e, GFP_KERNEL).is_err() {
            self.bits.set_trace_alloc_failed(true);
        }
    }

    #[cold]
    fn trace_to_text(&self, out: &mut Printbuf) {
        if self.trace.is_empty() {
            return;
        }

        write!(out, "Allocation attempts ({} total):\n", self.trace.len());
        let mut out = out.indent(2);
        for e in self.trace.iter() {
            let b = e.bits;
            match e.dev {
                u8::MAX => write!(out, "no dev"),
                dev     => write!(out, "dev {dev}"),
            };
            for (set, name) in [(b.new_stripe_alloc(),          " new_stripe"),
                                (b.will_retry_all_devices(),    " retry_all"),
                                (b.will_retry_target_devices(), " retry_target"),
                                (b.will_retry_set_devices(),    " retry_set"),
                                (b.have_cl(),                   " cl")] {
                if set {
                    write!(out, "{name}");
                }
            }
            write!(out, " free {} copygc_progress {}", e.free_buckets, u8::from(b.copygc_can_make_progress()));
            match e.err {
                0   => write!(out, " -> ok\n"),
                err => write!(out, " -> {}\n", BchError::from_raw(-i32::from(err))),
            };
        }
    }

    /// Its closure waiting on @list, to retry when it's woken - or already
    /// waiting: false if it has none, it's not to block.
    pub fn closure_wait(&self, list: &c::closure_waitlist) -> bool {
        let Some(cl) = NonNull::new(self.cl) else { return false };
        // SAFETY: its caller's closure, which outlives the request
        unsafe { closure_wait(list, cl) };
        true
    }
}

impl core::ops::Deref for AllocRequest<'_> {
    type Target = c::alloc_request;

    fn deref(&self) -> &c::alloc_request {
        self.0
    }
}

impl core::ops::DerefMut for AllocRequest<'_> {
    fn deref_mut(&mut self) -> &mut c::alloc_request {
        self.0
    }
}

impl Drop for AllocRequest<'_> {
    fn drop(&mut self) {
        self.0.trace_reset()
    }
}

// ── Waiting on the allocator ─────────────────────────────────────────────

impl Fs {
    /// Whether a wake @req's waiter woke to may be for it, and it should
    /// retry: a wake of everything, a trace that couldn't be kept, a device
    /// it failed on gone - or woken with buckets now free, or nothing copygc
    /// can do to free more. Woken with neither, copygc's kicked, and it
    /// waits again.
    fn alloc_wait_advanced(&self, req: &mut c::alloc_request) -> bool {
        if self.allocator.wake_all_counter.load(Relaxed) as u32 != req.wake_all_counter_snapshot ||
           req.bits.trace_alloc_failed() {
            return true;
        }

        let _mark = self.mark_lock().read();
        let rcu = read_lock();
        let mut found = false;

        for e in req.trace.iter().filter(|e| e.err != 0 && u32::from(e.dev) != c::BCH_SB_MEMBER_INVALID) {
            found = true;

            let Some(ca) = self.dev_rcu(e.dev.into(), &rcu) else { return true };
            if ca.alloc_wake_counter.load(Relaxed) as u32 != e.wake_counter_snapshot {
                req.usage = ca.usage_read_fast();
                if ca.free_buckets_at(&req.usage, req.watermark) > 1 || !can_make_progress(ca) {
                    return true;
                }
                self.copygc_wakeup();
            }
        }
        assert!(found, "waiting on the allocator with no device that failed in the trace");
        false
    }

    /// Whether @req could allocate from @ca: a device in its target, if it may
    /// only use those, read-write, online, and taking its data type.
    fn dev_may_alloc(&self, ca: &c::bch_dev, req: &c::alloc_request) -> bool {
        if req.flags.contains(c::bch_write_flags::BCH_WRITE_only_specified_devs) && req.target != 0 &&
           !self.dev_in_target(ca.dev_idx.into(), Target::decode(req.target), &read_lock()) {
            return false;
        }
        ca.is_rw() && ca.is_online() && u32::from(ca.mi.get().data_allowed) & 1 << req.data_type.0 != 0
    }

    #[cold]
    fn alloc_request_to_text(&self, out: &mut Printbuf, req: &c::alloc_request) {
        let b = req.bits;
        write!(out, "nr_replicas:\t{}\n", req.nr_replicas);
        write!(out, "target:\t");
        self.target_to_text(out, Target::decode(req.target));
        write!(out, "\nwatermark:\t{}\n", req.watermark.name());
        write!(out, "data_type:\t{}\n", data_type_str(req.data_type));
        write!(out, "flags:\t");
        req.flags.to_text(out);
        write!(out, "\nec:\t{}\n", u8::from(b.ec()));
        write!(out, "will_retry_all_devices:\t{}\n", u8::from(b.will_retry_all_devices()));
        write!(out, "will_retry_target_devices:\t{}\n", u8::from(b.will_retry_target_devices()));
        write!(out, "will_retry_set_devices:\t{}\n", u8::from(b.will_retry_set_devices()));
        write!(out, "copygc_can_make_progress:\t{}\n", u8::from(b.copygc_can_make_progress()));
        write!(out, "have_cl:\t{}\n", u8::from(!req.cl.is_null()));

        if let Some(devs) = req.devs_have().filter(|d| d.nr != 0) {
            write!(out, "devs_have:\t");
            for (i, &dev) in devs.data[..devs.nr as usize].iter().enumerate() {
                if i != 0 {
                    write!(out, " ");
                }
                self.member_name_to_text(out, dev.into());
            }
            write!(out, "\n");
        }

        write!(out, "devs_may_alloc:\t");
        for i in req.devs_may_alloc.d.iter() {
            write!(out, "{i} ");
        }
        write!(out, "\ndevs_sorted:\t");
        for i in &req.devs_sorted.data[..req.devs_sorted.nr as usize] {
            write!(out, "{i} ");
        }
        write!(out, "\nallocated:\t{}\n", req.nr_effective);

        req.trace_to_text(out);
    }

    fn dev_alloc_debug_header(out: &mut Printbuf, ca: &c::bch_dev) {
        write!(out, "Dev {} ({}): {}", ca, ca.dev_idx, member_state_str(ca.mi.get().state));
        if !ca.is_online() {
            write!(out, " (offline)");
        }
        write!(out, "\nData allowed:\t");
        data_types_to_text(out, ca.mi.get().data_allowed.into());
        write!(out, "\n");
        ca.alloc_debug_to_text(&mut out.indent(2));
        write!(out, "\n");
    }

    /// The allocator stuck dump, for @req, waiting with @err past
    /// allocator_stuck_timeout: what's blocking it - the devices and what
    /// should be freeing buckets on them, or the open buckets - and the
    /// journal, if it's throttled.
    #[cold]
    #[inline(never)]
    fn print_allocator_stuck(&self, req: &c::alloc_request, err: BchError) {
        let mut buf = Printbuf::new();
        let out = &mut buf;

        write!(out, "Allocator stuck? Waited for {} seconds, err {err}\n", self.opts.allocator_stuck_timeout.get());
        out.tabstop_push(28);
        write!(out, "Allocation:\n");
        self.alloc_request_to_text(&mut out.indent(2), req);
        write!(out, "\n");

        if err.matches(bch_errcode::BCH_ERR_bucket_alloc_blocked) {
            write!(out, "Allocator debug:\n");
            self.alloc_debug_to_text(&mut out.indent(2));
            write!(out, "\n");

            out.make_room_gfp(4096, c::GFP_KERNEL);

            let (mut free, mut need_discard, mut reserve) = (0u64, 0u64, 0u64);
            {
                let mut out = out.atomic();
                let rcu = read_lock();

                write!(out, "Devices eligible for allocation\n");
                for ca in self.devs_rcu(&rcu).filter(|ca| self.dev_may_alloc(ca, req)) {
                    Self::dev_alloc_debug_header(&mut out, ca);
                }
                write!(out, "Devices ineligible for allocation\n");
                for ca in self.devs_rcu(&rcu).filter(|ca| !self.dev_may_alloc(ca, req)) {
                    Self::dev_alloc_debug_header(&mut out, ca);
                }

                for ca in self.devs_rcu(&rcu) {
                    let u = ca.usage_read_fast();
                    let sectors = u64::from(ca.mi.get().bucket_size);
                    need_discard += sectors * u.buckets[c::bch_data_type::BCH_DATA_need_discard.0 as usize];
                    free         += sectors * u.buckets[c::bch_data_type::BCH_DATA_free.0 as usize];
                    reserve      += sectors * ca.buckets_reserved(c::bch_watermark::BCH_WATERMARK_stripe);
                }
            }

            if need_discard > free.saturating_sub(reserve * 4) {
                write!(out, "Discard debug:\n");
                self.discards_to_text(&mut out.indent(2));
            }

            write!(out, "Copygc debug:\n");
            self.copygc_wait_to_text(&mut out.indent(2));
            write!(out, "\n");
        }

        if err.matches(bch_errcode::BCH_ERR_open_bucket_alloc_blocked) {
            self.fs_open_buckets_to_text(out);
        }

        if self.journal.watermark() != c::bch_watermark::BCH_WATERMARK_stripe {
            write!(out, "Journal debug:\n");
            self.journal.to_text(&mut out.indent(2));
        }

        print_str(self, LogLevel::ERR, &mut buf);
    }
}

impl BtreeTrans<'_> {
    /// Asleep on @cl - on freelist_wait or open_buckets_wait, for @req's
    /// allocation that failed with @err - until there's reason to retry it:
    /// alloc_wait_advanced(), or for any wait but the freelist's, woken at
    /// all. Past allocator_stuck_timeout it's counted, and dumped - once in
    /// two minutes, by whichever waiter claims the dump. Its locks dropped
    /// first.
    fn sleep_on_allocator(&self, req: &mut c::alloc_request, err: BchError, cl: &Opaque<c::closure>) {
        let fs = self.fs();
        let blocked = err.matches(bch_errcode::BCH_ERR_bucket_alloc_blocked);
        let until = jiffies().wrapping_add(c_ulong::from(fs.opts.allocator_stuck_timeout.get()) * HZ);

        self.unlock();

        loop {
            // a wake can land before @cl's on the waitlist, so look before
            // sleeping - and unpark it, which takes it off
            if blocked && fs.alloc_wait_advanced(req) {
                fs.alloc_waiters_unpark();
                return;
            }

            let t = until.wrapping_sub(jiffies()) as c_long;
            if t > 0 && !self.closure_sync_timeout(cl, t) {
                // every one, though the dump below is one in two minutes
                fs.event_inc_trace(events::allocator_stuck, |out| {
                    write!(out, "err {err}\n");
                    fs.alloc_request_to_text(out, req);
                });

                let last = &fs.allocator.last_stuck;
                let old = last.load(Relaxed);
                if (old == 0 || time_after(jiffies(), (old as c_ulong).wrapping_add(HZ * 60 * 2))) &&
                   last.compare_exchange(old, jiffies() as usize, Relaxed, Relaxed).is_ok() {
                    fs.print_allocator_stuck(req, err);
                }
            }

            self.closure_sync_timeout(cl, c_long::MAX);

            // going emergency read-only, a draining device may never have
            // buckets free: waiting again could block read_only_work forever
            if fs.flag(c::bch_fs_flags::BCH_FS_emergency_ro) || !blocked || fs.alloc_wait_advanced(req) {
                return;
            }

            // freelist_wait is every device's: a wake for none @req tried
            // isn't for it, and it waits again
            // SAFETY: the caller's, live; synced, it's on no waitlist
            unsafe { closure_wait(&fs.allocator.freelist_wait, NonNull::new(cl.as_ptr()).expect("a closure")) };

            if fs.alloc_wait_advanced(req) {
                fs.alloc_waiters_unpark();
                return;
            }
        }
    }
}

// ── Debug ────────────────────────────────────────────────────────────────

fn waiting_to_text(list: &c::closure_waitlist) -> &'static str {
    if closure_has_waiters(list) { "waiting" } else { "empty" }
}

/// @v, a value per replica count from 1.
fn per_replicas_to_text(out: &mut Printbuf, name: &str, v: impl IntoIterator<Item = u64>) {
    write!(out, "{name}\n");
    for (i, v) in v.into_iter().enumerate() {
        write!(out, "  {}x\t{v}\n", i + 1);
    }
}

impl Fs {
    /// The open buckets allocated, by data type, and whether anything's
    /// waiting for one.
    #[cold]
    pub fn fs_open_buckets_to_text(&self, out: &mut Printbuf) {
        if out.nr_tabstops() == 0 {
            out.tabstop_push(24);
        }

        let a = &self.allocator;
        let mut nr = [0u32; c::BCH_DATA_NR as usize];
        for ob in a.open_buckets.iter().filter(|ob| ob.pin.load(Relaxed) != 0) {
            if let Some(n) = nr.get_mut(usize::from(ob.data_type.get())) {
                *n += 1;
            }
        }

        write!(out, "open buckets allocated\t{}\n",
               c::OPEN_BUCKETS_COUNT - u32::from(a.freelist_lock.open_buckets_nr_free.get()));
        write!(out, "open buckets total\t{}\n", c::OPEN_BUCKETS_COUNT);
        for (i, n) in nr.into_iter().enumerate().filter(|&(_, n)| n != 0) {
            write!(out, "open_buckets {}:\t{n}\n", data_type_str(c::bch_data_type(i as u32)));
        }
        write!(out, "open_buckets_wait\t{}\n", waiting_to_text(&a.open_buckets_wait));
    }

    /// Capacity, usage and reservations, and the free space by the replica
    /// count it's placeable at.
    #[cold]
    pub fn alloc_debug_to_text(&self, out: &mut Printbuf) {
        if out.nr_tabstops() == 0 {
            out.tabstop_push(24);
        }

        let b = percpu_field!(self.capacity.pcpu.get(), counters).sum_nonnegative();
        write!(out, "capacity\t{}\n",   self.capacity.capacity.get());
        write!(out, "used\t{}\n",       self.usage_read_short().used);
        write!(out, "reserved\t{}\n",   self.capacity.reserved.get());
        write!(out, "hidden\t{}\n",     b.usage.hidden);
        write!(out, "btree\t{}\n",      b.usage.btree);
        write!(out, "data\t{}\n",       b.usage.data);
        write!(out, "cached\t{}\n",     b.usage.cached);
        write!(out, "reserved\t{}\n",   b.usage.reserved);
        // Per replica count: an ENOSPC at 3x with raw space free needs this
        per_replicas_to_text(out, "online_reserved", b.online_reserved);

        // Computed, not read back: "cached" below is only what the
        // reservation fast path has left, and reads zero until a slot has run
        // dry once.
        let p = self.sectors_placeable();
        per_replicas_to_text(out, "sectors_available placeable", p.eventual);
        per_replicas_to_text(out, "sectors_available now", p.now);
        // the exclusive partition underneath, which sums to the free space
        per_replicas_to_text(out, "sectors_available slots",
            p.eventual.iter().zip(p.eventual[1..].iter().chain([&0])).map(|(n, above)| n.wrapping_sub(*above)));
        // what the pools hold, which lags until a recalc
        per_replicas_to_text(out, "sectors_available cached",
            self.capacity.sectors_available.iter().map(|s| s.load(Relaxed)));

        writeln!(out);
        write!(out, "freelist_wait\t{}\n", waiting_to_text(&self.allocator.freelist_wait));
        write!(out, "btree reserve cache\t{}\n", self.btree.reserve_cache.lock.lock().nr);
        writeln!(out);
    }
}

impl c::bch_dev {
    /// Its usage, its reserve at each watermark, and its open buckets and
    /// buckets to invalidate.
    #[cold]
    pub fn alloc_debug_to_text(&self, out: &mut Printbuf) {
        self.usage_to_text(out, &self.usage_full_read_fast());
        writeln!(out);

        write!(out, "reserves:\n");
        for w in (0..c::bch_watermark::BCH_WATERMARK_NR as u32).map(c::bch_watermark::from_bits) {
            write!(out, "{w}\t{}\r\n", self.buckets_reserved(w));
        }
        writeln!(out);

        out.tabstops(&[12, 16]);
        write!(out, "open buckets\t{}\r\n", self.nr_open_buckets.get() as i32);
        write!(out, "buckets to invalidate\t{}\r\n", self.should_invalidate_buckets(&self.usage_read_fast()));
    }
}

impl Fs {
    /// The open buckets - on @ca, or all of them.
    pub fn open_buckets_to_text(&self, out: &mut Printbuf, ca: Option<&c::bch_dev>) {
        let mut out = out.atomic();
        for ob in &self.allocator.open_buckets {
            let l = ob.lock.lock();
            if let Some(ob) = ob.open_ref(&l).filter(|ob| ca.is_none_or(|ca| ob.id().dev() == ca.dev_idx)) {
                ob.to_text(&mut out);
            }
        }
    }

    pub fn open_buckets_partial_to_text(&self, out: &mut Printbuf) {
        let mut out = out.atomic();
        let g = self.allocator.freelist_lock.lock();
        self.allocator.partial(&g).for_each(|ob| ob.to_text(&mut out));
    }
}

impl ToText for OpenBucketRef {
    fn to_text(&self, out: &mut Printbuf) {
        let bucket_size = u32::from(self.ca().mi.get().bucket_size);
        write!(out, "{} ref {} ", self.idx().get(), self.pin.load(Relaxed));
        c::bch_data_type(self.data_type.get().into()).to_text(out);
        let id = self.id();
        write!(out, " {}:{} gen {} allocated {}/{}", id.dev(), id.bucket(), id.generation(),
               bucket_size.wrapping_sub(self.sectors_free.get()), bucket_size);
        if let Some(s) = self.stripe() {
            write!(out, " ec idx {}", s.idx.load(Relaxed));
        }
        if self.flag(c::open_bucket::ON_PARTIAL_LIST) {
            write!(out, " partial");
        }
        writeln!(out);
    }
}

// ── For C ────────────────────────────────────────────────────────────────

/// What's built once the allocator is in place, before anything allocates:
/// the open buckets' freelist - every one but slot 0, the sentinel, never
/// handed out - and the write points' hash.
#[c_export]
pub fn bch2_fs_allocator_foreground_init(c: &Opaque<c::bch_fs>) {
    let fs = Fs::from_c(c);
    let a = &fs.allocator;
    {
        let mut g = a.freelist_lock.lock();
        let (free, nr) = g.parts();
        for ob in &a.open_buckets[1..] {
            ob.freelist.set(free.open_buckets_freelist);
            free.open_buckets_freelist = Some(ob.idx());
            *nr.open_buckets_nr_free.place() += 1;
        }
    }
    fs.write_points_init();
}

#[c_export]
pub fn bch2_alloc_wake_all(c: &Opaque<c::bch_fs>) {
    Fs::from_c(c).alloc_wake_all()
}

/// bch2_open_bucket_put()'s, for the last pin.
///
/// # Safety
/// @ob's last pin was put by the caller.
#[c_export]
pub unsafe fn __bch2_open_bucket_put(c: &Opaque<c::bch_fs>, ob: &c::open_bucket) {
    // SAFETY: open until it's closed, which this does
    Fs::from_c(c).open_bucket_close(unsafe { OpenBucketRef::from_c(ob) })
}

/// The stripes the buckets of @obs on @dev are blocks of, cancelled with
/// @err: the writes to @dev failed.
#[c_export]
pub fn bch2_open_bucket_write_error(c: &Opaque<c::bch_fs>, obs: &c::open_buckets, dev: c_uint, err: c_int) {
    let Err(err) = ret_to_result_void(err) else { panic!("a write error without an error") };
    obs.iter(Fs::from_c(c)).filter(|ob| u32::from(ob.id().dev()) == dev).filter_map(|ob| ob.stripe()).for_each(|s| s.err.set(err));
}

/// # Safety
/// @ob is open, and held open for the call.
#[c_export]
pub unsafe fn bch2_open_bucket_to_text(out: &mut Printbuf, _c: &Opaque<c::bch_fs>, ob: &c::open_bucket) {
    // SAFETY: the caller's
    unsafe { OpenBucketRef::from_c(ob) }.to_text(out)
}

/// bch2_wait_on_allocator()'s, with @cl on a waitlist.
#[c_export]
pub fn __bch2_wait_on_allocator(trans: &BtreeTrans<'_>, req: &mut c::alloc_request, err: c_int,
                                cl: &Opaque<c::closure>) {
    let Err(err) = ret_to_result_void(err) else { panic!("waiting on the allocator without an error") };
    trans.sleep_on_allocator(req, err, cl)
}

/// For C: a request, in @trans's memory - an ERR_PTR() if there's none -
/// put by alloc_request_put().
///
/// # Safety
/// @trans is in an attempt.
#[c_export]
#[allow(clippy::too_many_arguments)]
pub unsafe fn alloc_request_get(trans: &BtreeTrans<'_>, target: c_uint, erasure_code: c_uint,
                                devs_have: *mut c::bch_devs_list, nr_replicas: c_uint, ec_replicas: c_uint,
                                watermark: c::bch_watermark, flags: c::bch_write_flags,
                                cl: *mut c::closure) -> *mut c::alloc_request {
    // SAFETY: the caller's
    let t = unsafe { trans.attempt_in_progress() };
    let req = c::alloc_request::new(t.fs(), target, erasure_code != 0, devs_have, nr_replicas, ec_replicas,
                                    watermark, flags, cl);
    t.alloc_init(req).map_or_else(err_ptr, core::ptr::from_mut)
}

#[c_export]
pub fn alloc_request_put(req: &mut c::alloc_request) {
    req.trace_reset()
}

/// @dev's @bucket's slot in the open buckets' hash.
#[c_export]
pub fn bch2_open_bucket_hash(dev: c_uint, bucket: u64) -> c_uint {
    hash_64(Fold::of(&(dev, bucket)), c::OPEN_BUCKETS_COUNT.ilog2()) as c_uint
}
