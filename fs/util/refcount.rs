// SPDX-License-Identifier: GPL-2.0

//! Refcounts, and refs on them.
//!
//! Refcount<K> is a count on a T. A count that's a type of its own -
//! EnumeratedRef, c->writes and ca->io_ref[]; a closure's; an open bucket's
//! pins - is Refcount<()>, K's default. One that isn't, yet, is a count K on
//! the T that has it, K a marker type naming which: c->ro_ref is Fs's
//! Refcount<RoRefs>.
//!
//! A ref keeps what it counts alive - and what that's in: its owner waits
//! the count out before it frees it. That's what a refcount is, and it's
//! Refcount's contract, so a ref is all it takes to have the T: a RefGuard
//! has no lifetime, and goes wherever it's needed - into a bio in flight,
//! beside the box that owns what it counts.
//!
//! get() takes a ref that can't fail. A TryRefcount can also be tried for:
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
//! Refs are RefGuards: a RefGuard<T, K> is a ref on T's count K, and derefs
//! to the T - the count itself, for a count that's a type of its own.
//! Cloning one takes another, dropping it puts it.
//! They're had from the trait's get() and tryget()s, or - for a T with
//! more than one count, where those would be ambiguous - from the guard's,
//! named by an alias: RoRef::tryget(fs). What an implementation
//! supplies are the raw counting - inc(), dec(), try_inc() - which the
//! guards are made from.
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

use core::marker::PhantomData;
use core::ops::Deref;
use core::ptr::NonNull;

/// # Safety
/// inc() takes a ref that's held until dec() gives it back - and while one
/// is, the T isn't freed, nor what it's in.
pub unsafe trait Refcount<K = ()> {
    fn inc(&self);

    /// # Safety
    /// The caller holds a ref, and gives it up.
    unsafe fn dec(&self);

    fn get(&self) -> RefGuard<Self, K> {
        RefGuard::get(self)
    }
}

/// # Safety
/// A try_inc() or try_inc_live() that returns true takes a ref, as inc().
pub unsafe trait TryRefcount<K = ()>: Refcount<K> {
    fn try_inc(&self) -> bool;

    fn try_inc_live(&self) -> bool {
        self.try_inc()
    }

    fn tryget(&self) -> Option<RefGuard<Self, K>> {
        RefGuard::tryget(self)
    }

    fn tryget_live(&self) -> Option<RefGuard<Self, K>> {
        RefGuard::tryget_live(self)
    }
}

/// A ref on T's count K: T, kept by it.
#[must_use = "the ref is put as soon as it's dropped"]
pub struct RefGuard<T: Refcount<K> + ?Sized, K = ()> {
    t:  NonNull<T>,
    _k: PhantomData<fn() -> K>,
}

// SAFETY: a &T, for as long as the ref keeps T: Send and Sync as &T is
unsafe impl<T: Refcount<K> + ?Sized + Sync, K> Send for RefGuard<T, K> {}
unsafe impl<T: Refcount<K> + ?Sized + Sync, K> Sync for RefGuard<T, K> {}

impl<T: Refcount<K> + ?Sized, K> RefGuard<T, K> {
    pub fn get(t: &T) -> Self {
        t.inc();
        RefGuard { t: NonNull::from(t), _k: PhantomData }
    }

    /// A ref taken without a guard, handed over.
    ///
    /// # Safety
    /// The caller holds a ref on @t's count K, and gives it up to this.
    pub(crate) unsafe fn adopt(t: &T) -> Self {
        RefGuard { t: NonNull::from(t), _k: PhantomData }
    }

    /// Its ref handed over without a guard, to be adopt()ed - not put.
    pub(crate) fn leak(self) {
        core::mem::forget(self);
    }
}

impl<T: TryRefcount<K> + ?Sized, K> RefGuard<T, K> {
    // then(), lazily: a guard made when the try failed would put a ref on
    // its drop that nothing took
    pub fn tryget(t: &T) -> Option<Self> {
        t.try_inc().then(|| RefGuard { t: NonNull::from(t), _k: PhantomData })
    }

    pub fn tryget_live(t: &T) -> Option<Self> {
        t.try_inc_live().then(|| RefGuard { t: NonNull::from(t), _k: PhantomData })
    }
}

impl<T: Refcount<K> + ?Sized, K> Deref for RefGuard<T, K> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: our ref keeps it - Refcount's contract
        unsafe { self.t.as_ref() }
    }
}

impl<T: Refcount<K> + ?Sized, K> Clone for RefGuard<T, K> {
    fn clone(&self) -> Self {
        RefGuard::get(&**self)
    }
}

impl<T: Refcount<K> + ?Sized, K> Drop for RefGuard<T, K> {
    fn drop(&mut self) {
        // SAFETY: ours, from inc(), a try_inc() that succeeded, or adopt();
        // the ref keeps T till it's put
        unsafe { self.t.as_ref().dec() }
    }
}
