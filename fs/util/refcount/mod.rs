// SPDX-License-Identifier: GPL-2.0

//! Refcounts, and refs on them.
//!
//! A Refcount is a count on a T, and each count is a type of its own: a T
//! with two counts has a type for each, as a device's read and write io refs
//! are each a Placed count of their own.
//!
//! A ref keeps what it counts alive - and what that's in: its owner waits
//! the count out before it frees it. That's what a refcount is, and it's
//! Refcount's contract, so a ref is all it takes to have the T: a RefGuard
//! has no lifetime, and goes wherever it's needed - into a bio in flight,
//! beside the box that owns what it counts.
//!
//! A TryRefcount can be tried for:
//! tryget() fails once the count is shutting down - or before it's started:
//! what's wanted almost always. tryget_live() is for a count that's killed in
//! two steps, as a percpu_ref is: it still succeeds while killing's under
//! way, and fails only once the count is dead - for what must finish once
//! it's begun. A count killed in one step has nothing to tell them apart:
//! tryget_live() is tryget().
//!
//! A count with nothing to try - an open bucket's pins, the first made with
//! the bucket, the rest from one that's held - is a Refcount and not a
//! TryRefcount, rather than having a tryget() that can't fail.
//!
//! Refs are RefGuards: a RefGuard<T> is a ref on T's count, and derefs to
//! what it keeps, T's Target - the T, or for a count placed in what it keeps,
//! a Placed one, that: a DevRef is a &Dev. Cloning one takes another,
//! dropping it puts it. What an
//! implementation supplies are the raw counting - inc(), dec(), try_inc() -
//! which the guards are made from.
//!
//! inc() is unsafe: a count embedded in what it keeps alive can't tell from
//! &self whether that's being torn down. A ref is taken with tryget(), or
//! from one already held: a RefGuard's clone(), or get() on a RefBorrow - a
//! ref lent by whatever holds it, as an open bucket list lends its buckets.
//!
//! A ref handed across without a guard - into a C list, as an open bucket's
//! pins are, or to a work item queued (ReffedWork, os/*/workqueue.rs) - is
//! leak()ed and adopt()ed back, by what does the handing: the run adopts its
//! ref, put as the run ends.
//!
//! What's counted may be valid only while a ref is held - an open bucket,
//! held open by its pins; a device, whose removal waits for its io refs - so
//! a guard hands out its T only for as long as the guard is borrowed, and
//! leak() gives back nothing.

use core::marker::PhantomData;
use core::mem::{transmute_copy, ManuallyDrop};
use core::ops::Deref;
use core::ptr::{null_mut, NonNull};
use core::sync::atomic::{AtomicPtr, Ordering};
use crate::util::vstructs::NonZeroWord;
use pin_init::ZeroableOption;

mod atomic;
mod percpu;
mod placed;

pub use atomic::AtomicRef;
pub use percpu::{OnRelease, PercpuRef};

#[cfg(kernel)]
pub use kernel::sync::aref::{ARef, AlwaysRefCounted};
#[cfg(not(kernel))]
pub use bcachefs_shim::sync::aref::{ARef, AlwaysRefCounted};

/// # Safety
/// inc() takes a ref that's held until dec() gives it back - and while one
/// is, the T isn't freed, nor what it's in.
pub unsafe trait Refcount {
    /// What a ref on it keeps, as its guard derefs to it.
    type Target: ?Sized;

    fn target(&self) -> &Self::Target;

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
    /// A ref, unless it's shutting down: what's wanted almost always.
    fn try_inc(&self) -> bool;

    /// A ref, unless it's dead - its count at zero - so while it's shutting
    /// down too: rarely what's wanted. Unless the count tells the two apart,
    /// try_inc()'s.
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

/// A ref on T's count: T, kept by it.
#[must_use = "the ref is put as soon as it's dropped"]
#[repr(transparent)]
#[derive(cstruct_macros::NonZeroWord)]
pub struct RefGuard<T: Refcount + ?Sized> {
    t: NonNull<T>,
}

// SAFETY: a &T, for as long as the ref keeps T: Send and Sync as &T is
unsafe impl<T: Refcount + ?Sized + Sync> Send for RefGuard<T> {}
unsafe impl<T: Refcount + ?Sized + Sync> Sync for RefGuard<T> {}

// None is a null NonNull: all zeroes
unsafe impl<T: Refcount> ZeroableOption for RefGuard<T> {}

impl<T: Refcount + ?Sized> RefGuard<T> {
    pub unsafe fn get(t: &T) -> Self {
        unsafe { t.inc() };
        RefGuard { t: NonNull::from(t) }
    }

    pub fn borrow(&self) -> RefBorrow<'_, T> {
        RefBorrow { t: self.counted() }
    }

    /// The count itself, not what it keeps.
    pub fn counted(&self) -> &T {
        unsafe { self.t.as_ref() }
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
    type Target = T::Target;

    fn deref(&self) -> &T::Target {
        self.counted().target()
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
    type Target = T::Target;

    fn deref(&self) -> &T::Target {
        self.t.target()
    }
}

/// A ref no one holder has - a count's, put by the count's last put, from any
/// context: so taken through &, once, by whoever takes it first. A RefGuard
/// or a GatherRef: a guard of one word, a pointer's. Send and Sync as the ref
/// is.
#[derive(cstruct_macros::MaybeZeroable)]
pub struct RefSlot<P: NonZeroWord> {
    p:  AtomicPtr<()>,
    _p: PhantomData<P>,
}

impl<P: NonZeroWord> RefSlot<P> {
    const WORD: () = assert!(size_of::<P>() == size_of::<*mut ()>(), "a RefSlot of what isn't a pointer");

    pub const fn new() -> Self {
        RefSlot { p: AtomicPtr::new(null_mut()), _p: PhantomData }
    }

    /// @r, to hold till it's taken: the slot's empty.
    pub fn set(&self, r: P) {
        let () = Self::WORD;
        // a pointer, never null, as NonZeroWord says: the slot owns it now
        let p = unsafe { transmute_copy::<ManuallyDrop<P>, *mut ()>(&ManuallyDrop::new(r)) };
        let old = self.p.swap(p, Ordering::AcqRel);
        assert!(old.is_null(), "a RefSlot set twice");
    }

    /// Its ref, if it hasn't been taken.
    pub fn take(&self) -> Option<P> {
        let p = self.p.swap(null_mut(), Ordering::AcqRel);
        // set()'s, swapped out once: ours alone
        (!p.is_null()).then(|| unsafe { transmute_copy::<*mut (), P>(&p) })
    }
}

impl<P: NonZeroWord> Drop for RefSlot<P> {
    fn drop(&mut self) {
        drop(self.take());
    }
}

/// stop() puts the owner's ref, and tryget() fails from then on; wait()
/// returns once every other ref has been put.
pub trait WaitableRef: TryRefcount {
    fn stop(&self);
    fn wait(&self);
}
