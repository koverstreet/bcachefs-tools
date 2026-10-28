// SPDX-License-Identifier: GPL-2.0

//! Nocow locks. A nocow write overwrites data in place; a data update moving
//! that data meanwhile would copy what's being overwritten and point the
//! extent at the stale copy. So in-place writes lock their buckets for
//! update and data updates for copy: one kind shares a bucket, and excludes
//! the other.
//!
//! The table is fixed: BUCKET_NOCOW_LOCKS hash buckets of
//! NOCOW_LOCK_BUCKET_SIZE slots. A slot is a device bucket and a count -
//! positive for update holders, negative for copy - free at zero. Slots are
//! claimed and counts raised under the hash bucket's lock; release is an
//! atomic decrement, from anywhere - a write's endio - waking waiters at
//! zero.
//!
//! A key's buckets are locked in hash bucket order, so no two lockers each
//! wait on the other - except on a full hash bucket, where the locker drops
//! everything and waits for it to empty.
//!
//! C holds them by bucket - an in-place write till each bio's endio, a data
//! update from init to exit - through the c_exports below.

use crate::btree::bkey::{BkeySC, BkeyV};
use crate::btree::iter::BtreeTrans;
use crate::c;
use crate::c::bch_errcode;
use crate::alloc::buckets::{bucket_to_u64, u64_to_bucket};
use crate::data::ptrs::{BkeyVPtrs, Ptrs};
use crate::errcode::BchError;
use crate::fs::Fs;
use crate::sb::members::DevRef;
use crate::locking::Lock;
use crate::new_lock;
use crate::types::CacheAligned;
use crate::util::futex::EventCount;
use crate::util::hash::hash_64;
use crate::util::kernel::local_clock;
use crate::util::sleep::Sleep;
use crate::util::{Printbuf, ToText};
use crate::warn_on_once;
use arrayvec::ArrayVec;
use core::ffi::c_int;
use core::sync::atomic::{AtomicI32, AtomicU64, Ordering::{AcqRel, Relaxed}};
use cstruct_macros::c_export;
use pin_init::{pin_data, pin_init, pinned_drop, PinInit};

const NR_SLOTS: usize = c::NOCOW_LOCK_BUCKET_SIZE as usize;
const NR_BUCKETS: usize = c::BUCKET_NOCOW_LOCKS as usize;

/// A key's buckets: one per pointer, at most.
type Buckets = ArrayVec<u64, { c::BCH_BKEY_PTRS_MAX as usize }>;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NocowKind {
    /// Held by a data update, while it copies the data out
    Copy,
    /// Held by an in-place write
    Update,
}

impl NocowKind {
    /// C's flags: BUCKET_NOCOW_LOCK_UPDATE, or 0 for copy.
    fn from_flags(flags: c_int) -> Self {
        if flags != 0 { NocowKind::Update } else { NocowKind::Copy }
    }

    fn count(self) -> i32 {
        match self {
            NocowKind::Update => 1,
            NocowKind::Copy   => -1,
        }
    }
}

fn is_contended(e: &BchError) -> bool {
    e.matches(bch_errcode::BCH_ERR_nocow_trylock_contended)
}

// ── A hash bucket ────────────────────────────────────────────────────────

#[pin_data]
struct NocowLockBucket {
    _align: CacheAligned,
    /// Taken to claim a slot, or raise its count
    #[pin]
    lock:   Lock<c::spinlock_t>,
    /// Each slot's device bucket, as bucket_to_u64(): meaningful while its
    /// count is nonzero. Read without the lock by unlock and is_locked(),
    /// which only look for a bucket they know is held
    b:      [AtomicU64; NR_SLOTS],
    l:      [AtomicI32; NR_SLOTS],
    wait:   EventCount,
}

impl NocowLockBucket {
    fn new() -> impl PinInit<Self> {
        pin_init!(Self {
            _align: CacheAligned::default(),
            lock <- new_lock!(()),
            b:      [const { AtomicU64::new(0) }; NR_SLOTS],
            l:      [const { AtomicI32::new(0) }; NR_SLOTS],
            wait:   EventCount::new(),
        })
    }

    fn empty(&self) -> bool {
        self.l.iter().all(|l| l.load(Relaxed) == 0)
    }

    fn slot(&self, dev_bucket: u64) -> Option<usize> {
        self.b.iter().position(|b| b.load(Relaxed) == dev_bucket)
    }

    /// nocow_trylock_contended if held the other way, or the count would
    /// overflow; nocow_trylock_bucket_full if there's no free slot.
    fn trylock(&self, fs: &Fs, dev_bucket: u64, kind: NocowKind) -> Result<(), BchError> {
        let v = kind.count();
        let _g = self.lock.lock();

        let i = match self.slot(dev_bucket) {
            Some(i) => {
                let held = self.l[i].load(Relaxed);
                if held.signum() == -v.signum() {
                    return fs.throw(bch_errcode::BCH_ERR_nocow_trylock_contended);
                }
                i
            }
            None => {
                let i = self.l.iter().position(|l| l.load(Relaxed) == 0)
                    .ok_or_else(|| fs.err(bch_errcode::BCH_ERR_nocow_trylock_bucket_full))?;
                self.b[i].store(dev_bucket, Relaxed);
                i
            }
        };

        if self.l[i].load(Relaxed).checked_add(v).is_none() {
            return fs.throw(bch_errcode::BCH_ERR_nocow_trylock_contended);
        }
        self.l[i].fetch_add(v, AcqRel);
        Ok(())
    }

    fn unlock(&self, dev_bucket: u64, kind: NocowKind) {
        let v = kind.count();
        let i = self.slot(dev_bucket)
            .unwrap_or_else(|| panic!("nocow unlock of bucket {dev_bucket:#x}, which isn't locked"));
        let left = self.l[i].fetch_sub(v, AcqRel) - v;

        assert!(left == 0 || left.signum() == v.signum(),
                "nocow unlock of bucket {dev_bucket:#x}, held the other way: {left}");
        if left == 0 {
            self.wait.wake();
        }
    }

    /// @cond is a trylock, so it's called once per wake and its result kept -
    /// unlike EventCount::wait_until(), which calls it again before sleeping.
    fn wait_until<T>(&self, trans: Option<&BtreeTrans<'_>>, mut cond: impl FnMut() -> Option<T>) -> T {
        match trans {
            Some(t) => t.wait_until(&self.wait, cond),
            None    => loop {
                let seen = self.wait.snapshot();
                if let Some(v) = cond() {
                    return v;
                }
                let Ok(()) = self.wait.wait(seen, Sleep::UNINTERRUPTIBLE);
            },
        }
    }
}

// ── The table ────────────────────────────────────────────────────────────

#[pin_data(PinnedDrop)]
pub struct NocowLocks {
    #[pin]
    l: [NocowLockBucket; NR_BUCKETS],
}

impl NocowLocks {
    pub fn new() -> impl PinInit<Self> {
        pin_init!(Self { l <- pin_init::pin_init_array_from_fn(|_| NocowLockBucket::new()) })
    }

    fn bucket(&self, dev_bucket: u64) -> &NocowLockBucket {
        &self.l[hash_64(dev_bucket, c::BUCKET_NOCOW_LOCKS_BITS) as usize & (NR_BUCKETS - 1)]
    }

    pub fn is_locked(&self, bucket: c::bpos) -> bool {
        let dev_bucket = bucket_to_u64(bucket);
        let l = self.bucket(dev_bucket);
        l.slot(dev_bucket).is_some_and(|i| l.l[i].load(Relaxed) != 0)
    }

    /// Releases one of a key's buckets alone, as an in-place write's bio does
    /// when it completes.
    pub fn unlock_bucket(&self, bucket: c::bpos, kind: NocowKind) {
        let dev_bucket = bucket_to_u64(bucket);
        self.bucket(dev_bucket).unlock(dev_bucket, kind)
    }

    /// Locks all of @buckets, or none.
    pub fn trylock(&self, fs: &Fs, buckets: &[u64], kind: NocowKind) -> bool {
        for (n, &b) in buckets.iter().enumerate() {
            if self.bucket(b).trylock(fs, b, kind).is_err() {
                self.unlock(&buckets[..n], kind);
                return false;
            }
        }
        true
    }

    /// Locks @buckets, waiting if need be - with @trans, as a transaction
    /// waits, its btree locks dropped.
    pub fn lock(&self, fs: &Fs, trans: Option<&BtreeTrans<'_>>, buckets: &[u64], kind: NocowKind) {
        if self.trylock(fs, buckets, kind) {
            return;
        }

        // in hash bucket order: we never wait holding what another waits for
        let mut order: ArrayVec<(u64, &NocowLockBucket), { c::BCH_BKEY_PTRS_MAX as usize }> =
            buckets.iter().map(|&b| (b, self.bucket(b))).collect();
        order.sort_unstable_by_key(|&(_, l)| core::ptr::from_ref(l) as usize);
        warn_on_once!(order.len() > NR_SLOTS);

        let contended_stat = &fs.times[c::bch_time_stats::BCH_TIME_nocow_lock_contended as usize];
        'retake_all: loop {
            for (n, &(b, l)) in order.iter().enumerate() {
                let mut ret = l.trylock(fs, b, kind);
                if ret.is_ok() {
                    continue;
                }

                let start = local_clock();
                if ret.as_ref().is_err_and(is_contended) {
                    ret = l.wait_until(trans, || {
                        let r = l.trylock(fs, b, kind);
                        (!r.as_ref().is_err_and(is_contended)).then_some(r)
                    });
                }
                if ret.is_ok() {
                    contended_stat.update(start);
                    continue;
                }

                let e = ret.unwrap_err();
                assert!(e.matches(bch_errcode::BCH_ERR_nocow_trylock_bucket_full),
                        "nocow lock: unexpected error {e}");
                for &(b2, l2) in &order[..n] {
                    l2.unlock(b2, kind);
                }
                l.wait_until(trans, || l.empty().then_some(()));
                contended_stat.update(start);
                continue 'retake_all;
            }
            break;
        }
    }

    pub fn unlock(&self, buckets: &[u64], kind: NocowKind) {
        for &b in buckets {
            self.bucket(b).unlock(b, kind);
        }
    }
}

#[pinned_drop]
impl PinnedDrop for NocowLocks {
    fn drop(self: core::pin::Pin<&mut Self>) {
        for l in &self.l {
            assert!(l.empty(), "nocow locks still held as the filesystem goes");
        }
    }
}

impl ToText for NocowLocks {
    #[cold]
    fn to_text(&self, out: &mut Printbuf) {
        let mut nr_empty = 0;

        for l in &self.l {
            if l.empty() {
                nr_empty += 1;
                continue;
            }
            if nr_empty != 0 {
                write!(out, "({nr_empty} empty entries)\n");
            }
            nr_empty = 0;

            for (b, v) in l.b.iter().zip(&l.l) {
                let v = v.load(Relaxed);
                if v != 0 {
                    u64_to_bucket(b.load(Relaxed)).to_text(out);
                    write!(out, ": {} {} ", if v < 0 { "copy" } else { "update" }, v.unsigned_abs());
                }
            }
            out.newline();
        }

        if nr_empty != 0 {
            write!(out, "({nr_empty} empty entries)\n");
        }
    }
}

// ── For C ────────────────────────────────────────────────────────────────

/// The buckets of @k's pointers that @dev gives a device for, by pointer
/// index; a pointer without one isn't locked.
fn buckets<'a>(fs: &Fs, k: BkeySC<'_>, dev: impl Fn(usize) -> Option<&'a c::bch_dev>) -> Buckets {
    let Some(k) = k.arm::<Ptrs>() else { return Buckets::new() };
    k.ptrs(fs).enumerate()
        .filter_map(|(i, ptr)| dev(i).map(|ca| bucket_to_u64(ca.ptr_bucket_pos(ptr))))
        .collect()
}

/// buckets(), from C's cas[]: cas[i] is the device of @k's i'th pointer if
/// the caller holds a ref on it, or NULL.
///
/// # Safety
/// @cas has an entry per pointer of @k, each NULL or a device the caller
/// holds a ref on.
unsafe fn cas_buckets(fs: &Fs, k: BkeySC<'_>, cas: *const *mut c::bch_dev) -> Buckets {
    // SAFETY: the caller's
    buckets(fs, k, |i| unsafe { (*cas.add(i)).as_ref() })
}

fn devs_buckets(fs: &Fs, k: BkeySC<'_>, devs: &[Option<DevRef>]) -> Buckets {
    buckets(fs, k, |i| devs.get(i).and_then(|d| d.as_deref()))
}

/// A key's buckets, locked one way: dropping it unlocks them. They're worked
/// out once, as they're locked; unlocking needs nothing but them.
pub(crate) struct Locked {
    locks:   &'static NocowLocks,
    buckets: Buckets,
    kind:    NocowKind,
}

impl Drop for Locked {
    fn drop(&mut self) {
        self.locks.unlock(&self.buckets, self.kind)
    }
}

/// Locks @k's buckets on the devices in @devs, which is parallel to its
/// pointers - if they're all free to.
pub(crate) fn bkey_trylock(fs: &'static Fs, k: BkeySC<'_>, devs: &[Option<DevRef>], kind: NocowKind)
    -> Option<Locked>
{
    let buckets = devs_buckets(fs, k, devs);
    fs.nocow_locks.trylock(fs, &buckets, kind).then(|| Locked { locks: &fs.nocow_locks, buckets, kind })
}

/// bkey_trylock(), waiting as NocowLocks::lock() does.
pub(crate) fn bkey_lock(fs: &'static Fs, trans: Option<&BtreeTrans<'_>>, k: BkeySC<'_>, devs: &[Option<DevRef>],
                        kind: NocowKind) -> Locked {
    let buckets = devs_buckets(fs, k, devs);
    fs.nocow_locks.lock(fs, trans, &buckets, kind);
    Locked { locks: &fs.nocow_locks, buckets, kind }
}

/// # Safety
/// As cas_buckets(); @trans is NULL or the caller's, and waiting drops its
/// btree locks.
#[c_export]
pub unsafe fn bch2_bkey_nocow_lock(c: &Fs, trans: Option<&BtreeTrans<'_>>, k: BkeySC<'_>,
                                   cas: *const *mut c::bch_dev, flags: c_int) {
    // SAFETY: the caller's
    let buckets = unsafe { cas_buckets(c, k, cas) };
    c.nocow_locks.lock(c, trans, &buckets, NocowKind::from_flags(flags))
}

/// # Safety
/// As cas_buckets(), with @cas as it was when they were locked.
#[c_export]
pub unsafe fn bch2_bkey_nocow_unlock(c: &Fs, k: BkeySC<'_>, cas: *const *mut c::bch_dev, flags: c_int) {
    // SAFETY: the caller's
    let buckets = unsafe { cas_buckets(c, k, cas) };
    c.nocow_locks.unlock(&buckets, NocowKind::from_flags(flags))
}

#[c_export]
pub fn bch2_bucket_nocow_unlock(c: &Fs, bucket: c::bpos, flags: c_int) {
    c.nocow_locks.unlock_bucket(bucket, NocowKind::from_flags(flags))
}

#[c_export]
pub fn bch2_bucket_nocow_is_locked(c: &Fs, bucket: c::bpos) -> bool {
    c.nocow_locks.is_locked(bucket)
}
