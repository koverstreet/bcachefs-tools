// SPDX-License-Identifier: GPL-2.0

//! foreground.h's, for Rust: open buckets - a pin on one, and the hash that
//! says whether a bucket is open, being written - and allocation requests.
//!
//! An open bucket is a slot in the allocator's open_buckets[], named by its
//! index: a bucket handed out to be written, kept open by its pins - ->pin, a
//! refcount. The last put closes it: back on the freelist, or for a block of
//! a stripe being made, its ->ec's io ref dropped. Pins move by index - an
//! open_buckets list (a write point's, an alloc_request's) - and an
//! OpenBucket is one Rust holds, a RefGuard on the bucket's pins: into_idx()
//! hands it to such a list, from_idx() takes one back. While it's pinned, a bucket's dev, generation
//! and bucket don't change; sectors_free is its writer's.
//!
//! A block of a stripe being made that a writer has claimed is open past its
//! last put, for the stripe: an EcOpenBucket, the other owning handle, which
//! closes it. Both deref to an OpenBucketRef, the bucket borrowed - which is
//! also what one C holds a pin on is, borrowed from C: from_c(), from_idx().
//!
//! The hash is OPEN_BUCKETS_COUNT slots, each the head of a chain through
//! the open buckets' ->hash, 0 ending it, a bucket's slot hash_64() of its
//! (dev, bucket). C finds the slot here too - bch2_open_bucket_hash(), for
//! open_bucket_hashslot() - so the two can't disagree.
//!
//! It's written under freelist_lock and read without it, as C's
//! bch2_bucket_is_open() does: every slot and field once, volatile, as C's
//! plain loads. A bucket opened or closed while we look may be seen or
//! missed: a caller for whom that matters looks again under the lock, as
//! bch2_bucket_is_open_safe().

use crate::btree::iter::TransAttempt;
use crate::c;
use crate::errcode::{errptr_to_result, ret_to_result_void, BchError};
use crate::fs::Fs;
use crate::util::closure::StackClosure;
use crate::util::ffi::Opaque;
use crate::util::hash::{hash_64, Fold};
use crate::util::place::{self, Placed};
use crate::util::rcu::read_lock;
use crate::util::refcount::{RefGuard, Refcount};
use core::ffi::{c_int, c_uint};
use core::ops::Deref;
use core::ptr::{read_volatile, NonNull};
use core::sync::atomic::Ordering::{Relaxed, SeqCst};
use cstruct_macros::{c_export, c_extern};

const _: () = assert!(c::OPEN_BUCKETS_COUNT.is_power_of_two());

c_extern! {
    pub fn __bch2_open_bucket_put(c: *mut c::bch_fs, ob: *mut c::open_bucket);
    pub fn bch2_bucket_alloc_set_trans(trans: *mut c::btree_trans, req: *mut c::alloc_request,
                                       stripe: *mut c::dev_stripe_state) -> c_int;
    pub fn bch2_bucket_alloc_trans(trans: *mut c::btree_trans, req: *mut c::alloc_request) -> *mut c::open_bucket;
    pub fn bch2_alloc_waiters_unpark(c: *mut c::bch_fs);
    pub fn bch2_dev_alloc_list(c: *mut c::bch_fs, stripe: *mut c::dev_stripe_state, req: *mut c::alloc_request);
    pub fn __bch2_wait_on_allocator(trans: *mut c::btree_trans, req: *mut c::alloc_request, err: c_int,
                                    cl: *mut c::closure);
    pub fn alloc_request_get(trans: *mut c::btree_trans, target: c_uint, erasure_code: c_uint,
                             devs_have: *mut c::bch_devs_list, nr_replicas: c_uint, ec_replicas: c_uint,
                             watermark: c::bch_watermark, flags: c::bch_write_flags,
                             cl: *mut c::closure) -> *mut c::alloc_request;
    pub fn alloc_request_put(req: *mut c::alloc_request);
}

macro_rules! watermark_strs {
    ($(($n:ident)),* $(,)?) => { [$(stringify!($n)),*] };
}
const WATERMARK_STRS: [&str; c::bch_watermark::BCH_WATERMARK_NR as usize] = c::BCH_WATERMARKS!(watermark_strs);

impl c::bch_watermark {
    pub fn name(self) -> &'static str {
        WATERMARK_STRS.get(self as usize).copied().unwrap_or("(invalid)")
    }
}

// ── Open buckets ─────────────────────────────────────────────────────────

/// C writes through it while it's borrowed - @sectors_free is its writer's -
/// so it's an Opaque, as MoveStats; what's read when is open_bucket's
/// (alloc/types.rs).
#[repr(transparent)]
pub struct OpenBucketRef(Opaque<c::open_bucket>);

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
        // SAFETY: a slot of the allocator's table; the caller's
        unsafe { Self::from_c(&raw const (*fs.as_ptr()).allocator.open_buckets[idx.get()]) }
    }

    fn raw(&self) -> *mut c::open_bucket {
        self.0.as_ptr()
    }

    pub fn idx(&self) -> c::OpenBucketIdx {
        // SAFETY: its place's - written as the table was built, and fixed
        // since - and not 0: an open bucket isn't slot 0, the sentinel, which
        // is never handed out
        unsafe { c::OpenBucketIdx::new_unchecked((*self.raw()).idx.get()) }
    }

    /// The allocator whose table it's a slot of: its place's container().
    fn container(&self) -> &c::bch_fs_allocator {
        // SAFETY: an open bucket - from_c()'s - so a live open_bucket,
        // initialized. Widened from the Opaque: all of it interior mutable
        unsafe { place::container_of(self.raw()) }
    }

    /// Its filesystem: its allocator's.
    fn fs(&self) -> &Fs {
        self.container().container()
    }

    pub fn dev(&self) -> u32 {
        // SAFETY: fixed while it's open
        unsafe { (*self.raw()).dev }.into()
    }

    pub fn generation(&self) -> u8 {
        // SAFETY: fixed while it's open
        unsafe { (*self.raw()).generation }
    }

    pub fn bucket(&self) -> u64 {
        // SAFETY: fixed while it's open
        unsafe { (*self.raw()).bucket }
    }

    /// Its free space - its writer's, while it's being written: read once.
    pub fn sectors_free(&self) -> u32 {
        // SAFETY: read once, volatile, as C's
        unsafe { read_volatile(&raw const (*self.raw()).sectors_free) }
    }

    /// Its pins, as of now.
    pub fn pin(&self) -> i32 {
        // SAFETY: atomic
        unsafe { (*self.raw()).pin.load(Relaxed) }
    }

    /// The stripe being made that it's a block of, once a writer's claimed
    /// it: null if none.
    pub fn ec(&self) -> *mut c::ec_stripe_new {
        // SAFETY: set before its holders have it, cleared when the stripe
        // closes it
        unsafe { (*self.raw()).ec }
    }

    /// Which block of @ec it is.
    pub fn ec_idx(&self) -> u8 {
        // SAFETY: as ec()'s
        unsafe { (*self.raw()).ec_idx }
    }
}

// SAFETY: a pin is any thread's to hold and put: ->pin is atomic, the last
// put takes the allocator's locks, and what's read while it's pinned is fixed
// or read once, volatile
unsafe impl Sync for OpenBucketRef {}

// SAFETY: an OpenBucketRef is had only while it's held open - by a pin, or
// its ->ec - so inc() is from a pin that's held; the last put closes it
unsafe impl Refcount for OpenBucketRef {
    fn inc(&self) {
        // SAFETY: its pin field, atomic: atomic_inc()
        unsafe { (*self.raw()).pin.fetch_add(1, Relaxed) };
    }

    unsafe fn dec(&self) {
        // SAFETY: its pin field, atomic; the caller's to put. Fully ordered,
        // as atomic_dec_and_test().
        unsafe {
            if (*self.raw()).pin.fetch_sub(1, SeqCst) == 1 {
                __bch2_open_bucket_put(self.fs().as_ptr(), self.raw());
            }
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

    /// Claimed by the writer of block @ec_idx of stripe @ec: its pin the
    /// writer's - the bucket, for it - and its ->ec what keeps it open for the
    /// stripe.
    pub fn claim(self, ec: *mut c::ec_stripe_new, ec_idx: u8) -> (EcOpenBucket, *mut c::open_bucket) {
        let ob = NonNull::from(&*self).cast::<c::open_bucket>();
        self.leak();
        // SAFETY: ->ec and ->ec_idx are the pin's holder's to set, before the
        // bucket's handed on; its writer reads them once it has it
        unsafe {
            (*ob.as_ptr()).ec_idx = ec_idx;
            (*ob.as_ptr()).ec = ec;
        }
        (EcOpenBucket { ob }, ob.as_ptr())
    }

    /// Where its free space starts.
    pub fn ptr(&self) -> c::bch_extent_ptr {
        let rcu = read_lock();
        let ca = self.fs().dev_rcu(self.dev(), &rcu).expect("an open bucket's device");

        c::bch_extent_ptr::new()
            .with_generation(self.generation())
            .with_dev(self.dev() as u8)
            .with_offset(ca.bucket_to_sector(self.bucket()) + u64::from(ca.mi.bucket_size) -
                         u64::from(self.sectors_free()))
    }
}

/// Closed when it's dropped, once the stripe's done with it:
/// __bch2_open_bucket_put() with ->ec cleared.
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
        let ob = self.ob.as_ptr();
        // SAFETY: its pin field, atomic
        let pin = unsafe { (*ob).pin.load(SeqCst) };
        assert_eq!(pin, 0, "closing open bucket {}, claimed by a writer still holding its pin", self.idx().get());
        // SAFETY: open only for its stripe, which this ends
        unsafe {
            (*ob).ec = core::ptr::null_mut();
            __bch2_open_bucket_put(self.fs().as_ptr(), ob);
        }
    }
}

impl c::open_buckets {
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

// ── Allocating ───────────────────────────────────────────────────────────

impl<'a, 't> TransAttempt<'a, 't> {
    /// Into req.ptrs - pins it holds, including those it got before an
    /// error - @stripe spreading them across req.devs_may_alloc.
    pub fn bucket_alloc_set(&self, req: &mut c::alloc_request, stripe: &mut c::dev_stripe_state)
        -> Result<(), BchError>
    {
        // SAFETY: in the attempt; @req and @stripe ours
        ret_to_result_void(unsafe { bch2_bucket_alloc_set_trans(self.raw(), req, stripe) })
    }

    /// A bucket for @req on @ca.
    pub fn bucket_alloc(&self, req: &mut c::alloc_request, ca: &c::bch_dev) -> Result<OpenBucket, BchError> {
        req.ca = core::ptr::from_ref(ca).cast_mut();
        // SAFETY: in the attempt; @req ours, its device live while it's borrowed
        let ob = unsafe { bch2_bucket_alloc_trans(self.raw(), req) };
        req.ca = core::ptr::null_mut();

        let ob = errptr_to_result(NonNull::new(ob).expect("a bucket or an error"))?;
        // SAFETY: a slot in the table, the pin returned with it ours - not
        // slot 0, the sentinel, which is never handed out
        unsafe { Ok(OpenBucket::from_idx(self.fs(), c::OpenBucketIdx::new_unchecked((*ob.as_ptr()).idx.get()))) }
    }
}

impl Fs {
    /// Wake every allocation waiting on the freelist, so one of ours that's
    /// giving up can get off it - the rest see no new buckets, and wait
    /// again.
    pub fn alloc_waiters_unpark(&self) {
        // SAFETY: plain
        unsafe { bch2_alloc_waiters_unpark(self.as_ptr()) }
    }

    /// @req's devs_sorted: its devs_may_alloc, in the order @stripe spreads
    /// allocations across them.
    pub fn dev_alloc_list(&self, stripe: &mut c::dev_stripe_state, req: &mut c::alloc_request) {
        // SAFETY: plain; both ours
        unsafe { bch2_dev_alloc_list(self.as_ptr(), stripe, req) }
    }
}

// ── Requests ─────────────────────────────────────────────────────────────

/// An allocation request, in transaction memory - an attempt's - made and
/// put by C, which owns its trace darray.
pub struct AllocRequest<'a>(&'a mut c::alloc_request);

impl<'a, 't> TransAttempt<'a, 't> {
    /// Erasure coding only for @ec_replicas of 2 or more.
    #[allow(clippy::too_many_arguments)]
    pub fn alloc_request(&self, target: u32, erasure_code: bool, devs_have: *mut c::bch_devs_list,
                         nr_replicas: u32, ec_replicas: u32, watermark: c::bch_watermark,
                         flags: c::bch_write_flags, cl: *mut c::closure) -> Result<AllocRequest<'a>, BchError> {
        // SAFETY: in the attempt; the request is in its memory, ours until
        // it's put
        let req = unsafe {
            alloc_request_get(self.raw(), target, erasure_code.into(), devs_have, nr_replicas, ec_replicas,
                              watermark, flags, cl)
        };
        let req = errptr_to_result(NonNull::new(req).expect("a request or an error"))?;
        // SAFETY: just made, in the attempt's memory
        Ok(AllocRequest(unsafe { &mut *req.as_ptr() }))
    }

    /// Only if @cl went on a waitlist for it - @err the allocation's error.
    /// Then emergency_ro if the filesystem's gone emergency read-only, which
    /// won't allocate again; else it's for the caller to retry.
    pub fn wait_on_allocator(&self, req: &mut c::alloc_request, err: BchError, cl: &StackClosure)
        -> Result<(), BchError>
    {
        if cl.nr_remaining() > 1 {
            // SAFETY: in the attempt; @req and @cl ours
            unsafe { __bch2_wait_on_allocator(self.raw(), req, -err.raw(), cl.as_ptr()) };
        }
        let fs = self.fs();
        if fs.flag(c::bch_fs_flags::BCH_FS_emergency_ro) {
            return fs.throw(crate::c::bch_errcode::BCH_ERR_emergency_ro);
        }
        Ok(())
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
        // SAFETY: ours, from alloc_request_get(), put once
        unsafe { alloc_request_put(self.0) }
    }
}

/// @dev's @bucket's slot in the open buckets' hash.
#[c_export]
pub fn bch2_open_bucket_hash(dev: c_uint, bucket: u64) -> c_uint {
    hash_64(Fold::of(&(dev, bucket)), c::OPEN_BUCKETS_COUNT.ilog2()) as c_uint
}

impl Fs {
    /// Whether @dev's @bucket is open: bch2_bucket_is_open().
    pub fn bucket_is_open(&self, dev: u32, bucket: u64) -> bool {
        let slot = bch2_open_bucket_hash(dev, bucket) as usize;

        // SAFETY: the allocator's, live with the filesystem; each read whole,
        // and racing the writer as C's do
        unsafe {
            let a = &raw const (*self.as_ptr()).allocator;
            let mut ob_idx = read_volatile(&raw const (*a).open_buckets_hash[slot]);
            while let Some(i) = ob_idx {
                let ob = &raw const (*a).open_buckets[i.get()];
                if u32::from(read_volatile(&raw const (*ob).dev)) == dev &&
                   read_volatile(&raw const (*ob).bucket) == bucket {
                    return true;
                }
                ob_idx = read_volatile(&raw const (*ob).hash);
            }
        }
        false
    }
}
