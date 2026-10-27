// SPDX-License-Identifier: GPL-2.0

//! Refcounts, and refs on them.
//!
//! Refcount<K> is a count on a T. A count that's a type of its own -
//! EnumeratedRef, c->writes and ca->io_ref[]; a closure's; an open bucket's
//! pins - is Refcount<()>, K's default. One that isn't, yet, is a count K on
//! the T that has it, K a marker type naming which: c->ro_ref is Fs's
//! Refcount<RoRefs>. A count whose refs keep what it's in from being freed is
//! KeepsAlive: an EnumeratedRef, its owner stopping it before it frees it.
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
//! ref, put as the run ends. What the run finds from where the work is must
//! be kept by the ref, so its count is KeepsAlive.
//!
//! A guard's borrow needn't outlive its ref: what's counted may be valid
//! only while a ref is held - an open bucket, held open by its pins; a
//! device, whose removal waits for its io refs. So a guard hands out its T
//! only for as long as it's borrowed, and leak() gives back nothing.

use core::marker::PhantomData;
use core::ops::Deref;

/// # Safety
/// inc() takes a ref that's held until dec() gives it back.
pub unsafe trait Refcount<K = ()> {
    fn inc(&self);

    /// # Safety
    /// The caller holds a ref, and gives it up.
    unsafe fn dec(&self);

    fn get(&self) -> RefGuard<'_, Self, K> {
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

    fn tryget(&self) -> Option<RefGuard<'_, Self, K>> {
        RefGuard::tryget(self)
    }

    fn tryget_live(&self) -> Option<RefGuard<'_, Self, K>> {
        RefGuard::tryget_live(self)
    }
}

/// # Safety
/// While a ref on count K is held, the T isn't freed - nor anything it's in.
pub unsafe trait KeepsAlive<K = ()>: Refcount<K> {}

#[must_use = "the ref is put as soon as it's dropped"]
pub struct RefGuard<'a, T: Refcount<K> + ?Sized, K = ()> {
    t:  &'a T,
    _k: PhantomData<fn() -> K>,
}

impl<'a, T: Refcount<K> + ?Sized, K> RefGuard<'a, T, K> {
    pub fn get(t: &'a T) -> Self {
        t.inc();
        RefGuard { t, _k: PhantomData }
    }

    /// A ref taken without a guard, handed over.
    ///
    /// # Safety
    /// The caller holds a ref on @t's count K, and gives it up to this.
    pub(crate) unsafe fn adopt(t: &'a T) -> Self {
        RefGuard { t, _k: PhantomData }
    }

    /// Its ref handed over without a guard, to be adopt()ed - not put.
    pub(crate) fn leak(self) {
        core::mem::forget(self);
    }
}

impl<'a, T: TryRefcount<K> + ?Sized, K> RefGuard<'a, T, K> {
    // then(), lazily: a guard made when the try failed would put a ref on
    // its drop that nothing took
    pub fn tryget(t: &'a T) -> Option<Self> {
        t.try_inc().then(|| RefGuard { t, _k: PhantomData })
    }

    pub fn tryget_live(t: &'a T) -> Option<Self> {
        t.try_inc_live().then(|| RefGuard { t, _k: PhantomData })
    }
}

impl<T: Refcount<K> + ?Sized, K> Deref for RefGuard<'_, T, K> {
    type Target = T;

    fn deref(&self) -> &T {
        self.t
    }
}

impl<T: Refcount<K> + ?Sized, K> Clone for RefGuard<'_, T, K> {
    fn clone(&self) -> Self {
        RefGuard::get(self.t)
    }
}

impl<T: Refcount<K> + ?Sized, K> Drop for RefGuard<'_, T, K> {
    fn drop(&mut self) {
        // SAFETY: ours, from inc(), a try_inc() that succeeded, or adopt()
        unsafe { self.t.dec() }
    }
}
