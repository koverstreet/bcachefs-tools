// SPDX-License-Identifier: GPL-2.0

//! Refcounts, and refs on them.
//!
//! A Refcount is a count on a T, and each count is a type of its own: a T
//! with two counts has a type for each, as a device has DevIo<R> for its read
//! and write io refs.
//!
//! A ref keeps what it counts alive - and what that's in: its owner waits
//! the count out before it frees it. That's what a refcount is, and it's
//! Refcount's contract, so a ref is all it takes to have the T: a RefGuard
//! has no lifetime, and goes wherever it's needed - into a bio in flight,
//! beside the box that owns what it counts.
//!
//! A TryRefcount can be tried for:
//! tryget() fails once the count is dead - or has never started.
//! tryget_live() is for a count that's killed in two steps, as a percpu_ref
//! is: it fails as soon as killing begins, where tryget() still succeeds
//! until the count is dead - for what must finish once it's begun. A count
//! killed in one step has nothing to tell them apart: tryget_live() is
//! tryget().
//!
//! A count with nothing to try - an open bucket's pins, the first made with
//! the bucket, the rest from one that's held - is a Refcount and not a
//! TryRefcount, rather than having a tryget() that can't fail.
//!
//! A TestRefcount's put can tell it was the last: put_and_test(), for a count
//! whose last put is where what it counts ends - a bio's __bi_remaining. It's
//! a trait of its own as a percpu count can't tell without going atomic.
//!
//! Refs are RefGuards: a RefGuard<T> is a ref on T's count, and derefs to
//! the T. Cloning one takes another, dropping it puts it. What an
//! implementation supplies are the raw counting - inc(), dec(), try_inc() -
//! which the guards are made from.
//!
//! inc() is unsafe: a count embedded in what it keeps alive can't tell from
//! &self whether that's being torn down. A ref is taken with tryget(), or
//! from one already held: a RefGuard's clone(), or get() on a RefBorrow - a
//! ref lent by whatever holds it, as an open bucket list lends its buckets.
//!
//! A ref handed across without a guard - into a C list, as an open bucket's
//! pins are, or to a work item queued (ReffedWork, util/workqueue.rs) - is
//! leak()ed and adopt()ed back, by what does the handing: the run adopts its
//! ref, put as the run ends.
//!
//! What's counted may be valid only while a ref is held - an open bucket,
//! held open by its pins; a device, whose removal waits for its io refs - so
//! a guard hands out its T only for as long as the guard is borrowed, and
//! leak() gives back nothing.

use core::ops::Deref;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU32, Ordering::{Acquire, Relaxed, Release}};

use crate::util::futex::EventCount;
use crate::util::sleep::Sleep;

/// # Safety
/// inc() takes a ref that's held until dec() gives it back - and while one
/// is, the T isn't freed, nor what it's in.
pub unsafe trait Refcount {
    /// # Safety
    /// What the count is in stays alive while the new ref is held.
    unsafe fn inc(&self);

    /// # Safety
    /// The caller holds a ref, and gives it up.
    unsafe fn dec(&self);
}

/// # Safety
/// A try_inc() or try_inc_live() that returns true takes a ref, as inc().
pub unsafe trait TryRefcount: Refcount {
    fn try_inc(&self) -> bool;

    fn try_inc_live(&self) -> bool {
        self.try_inc()
    }

    fn tryget(&self) -> Option<RefGuard<Self>> {
        RefGuard::tryget(self)
    }

    fn tryget_live(&self) -> Option<RefGuard<Self>> {
        RefGuard::tryget_live(self)
    }
}

/// # Safety
/// dec_and_test() puts a ref, as dec(), and is true for the last.
pub unsafe trait TestRefcount: Refcount {
    unsafe fn dec_and_test(&self) -> bool;
}

/// A ref on T's count: T, kept by it.
#[must_use = "the ref is put as soon as it's dropped"]
pub struct RefGuard<T: Refcount + ?Sized> {
    t: NonNull<T>,
}

// SAFETY: a &T, for as long as the ref keeps T: Send and Sync as &T is
unsafe impl<T: Refcount + ?Sized + Sync> Send for RefGuard<T> {}
unsafe impl<T: Refcount + ?Sized + Sync> Sync for RefGuard<T> {}

impl<T: Refcount + ?Sized> RefGuard<T> {
    pub unsafe fn get(t: &T) -> Self {
        unsafe { t.inc() };
        RefGuard { t: NonNull::from(t) }
    }

    pub fn borrow(&self) -> RefBorrow<'_, T> {
        RefBorrow { t: &**self }
    }

    /// A ref taken without a guard, handed over.
    ///
    /// # Safety
    /// The caller holds a ref on @t's count, and gives it up to this.
    pub(crate) unsafe fn adopt(t: &T) -> Self {
        RefGuard { t: NonNull::from(t) }
    }

    /// Its ref handed over without a guard, to be adopt()ed - not put.
    pub(crate) fn leak(self) {
        core::mem::forget(self);
    }
}

impl<T: TestRefcount + ?Sized> RefGuard<T> {
    pub fn put_and_test(self) -> bool {
        let t = core::mem::ManuallyDrop::new(self).t;
        unsafe { t.as_ref().dec_and_test() }
    }
}

impl<T: TryRefcount + ?Sized> RefGuard<T> {
    // then(), lazily: a guard made when the try failed would put a ref on
    // its drop that nothing took
    pub fn tryget(t: &T) -> Option<Self> {
        t.try_inc().then(|| RefGuard { t: NonNull::from(t) })
    }

    pub fn tryget_live(t: &T) -> Option<Self> {
        t.try_inc_live().then(|| RefGuard { t: NonNull::from(t) })
    }
}

impl<T: Refcount + ?Sized> Deref for RefGuard<T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: our ref keeps it - Refcount's contract
        unsafe { self.t.as_ref() }
    }
}

impl<T: Refcount + ?Sized> Clone for RefGuard<T> {
    fn clone(&self) -> Self {
        self.borrow().get()
    }
}

impl<T: Refcount + ?Sized> Drop for RefGuard<T> {
    fn drop(&mut self) {
        // SAFETY: ours, from inc(), a try_inc() that succeeded, or adopt();
        // the ref keeps T till it's put
        unsafe { self.t.as_ref().dec() }
    }
}

pub struct RefBorrow<'a, T: Refcount + ?Sized> {
    t: &'a T,
}

impl<'a, T: Refcount + ?Sized> RefBorrow<'a, T> {
    /// # Safety
    /// A ref on @t's count is held for all of 'a.
    pub unsafe fn new(t: &'a T) -> Self {
        RefBorrow { t }
    }

    pub fn get(&self) -> RefGuard<T> {
        unsafe { RefGuard::get(self.t) }
    }

    pub fn into_ref(self) -> &'a T {
        self.t
    }
}

impl<T: Refcount + ?Sized> Clone for RefBorrow<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: Refcount + ?Sized> Copy for RefBorrow<'_, T> {}

impl<T: Refcount + ?Sized> Deref for RefBorrow<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.t
    }
}

/// stop() puts the owner's ref, and tryget() fails from then on; wait()
/// returns once every other ref has been put.
pub trait WaitableRef: TryRefcount {
    fn stop(&self);
    fn wait(&self);
}

/// A count and its value, as an Arc's, but not owning its memory: it starts
/// at one, the owner's ref, and the owner frees it after wait(). For slow
/// paths: EnumeratedRef is the percpu one.
#[repr(C)]
pub struct AtomicRef<T> {
    count:  AtomicU32,
    events: EventCount,
    value:  T,
}

const STOPPED: u32 = 1 << 31;

impl<T> AtomicRef<T> {
    pub const fn new(value: T) -> Self {
        AtomicRef { count: AtomicU32::new(1), events: EventCount::new(), value }
    }
}

impl<T> Deref for AtomicRef<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.value
    }
}

unsafe impl<T> Refcount for AtomicRef<T> {
    unsafe fn inc(&self) {
        self.count.fetch_add(1, Relaxed);
    }

    unsafe fn dec(&self) {
        if self.count.fetch_sub(1, Release) == STOPPED | 1 {
            self.events.wake();
        }
    }
}

unsafe impl<T> TryRefcount for AtomicRef<T> {
    fn try_inc(&self) -> bool {
        self.count.fetch_update(Relaxed, Relaxed, |r| (r & STOPPED == 0).then_some(r + 1)).is_ok()
    }
}

impl<T> WaitableRef for AtomicRef<T> {
    fn stop(&self) {
        if self.count.fetch_update(Release, Relaxed, |r| (r & STOPPED == 0).then_some((r - 1) | STOPPED))
               == Ok(1) {
            self.events.wake();
        }
    }

    fn wait(&self) {
        let Ok(()) = self.events.wait_until(Sleep::UNINTERRUPTIBLE, || {
            (self.count.load(Acquire) == STOPPED).then_some(())
        });
    }
}
