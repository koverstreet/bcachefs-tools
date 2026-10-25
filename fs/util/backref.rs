// SPDX-License-Identifier: GPL-2.0

//! BackRef<T>: a pointer to something that outlives it, which it can't
//! borrow.
//!
//! The case: an object owned by T - directly, or through a container T owns -
//! points back at T. A stripe being made points at its filesystem, which owns
//! it. A lifetime can't say that: T would contain things borrowing T. What
//! makes it sound is an ordering Rust can't see - T's owner drains what holds
//! a BackRef before T goes - so the constructor is unsafe, and that ordering
//! is its contract, promised once where the BackRef is made rather than at
//! every use.
//!
//! What's reached through the BackRef is another matter: get() is a
//! &'static T, for what its holder keeps beside it and hands T to by
//! reference - a stripe being made's disk reservation, its pins on open
//! buckets. new()'s caller can't promise where safe code puts that, so get()
//! is unsafe: its caller promises what it goes into is drained with the
//! holder.
//!
//! Every access is through a shared reference, and so must every other
//! access to T be while BackRefs exist: a BackRef hands out &T whenever it's
//! asked, and that's only sound alongside other &T - T's mutation interior,
//! as Fs's is.
//!
//! This is the stopgap. What makes it safe is in the design notes,
//! rust-contexts-and-backrefs: a #[context] declaring where a type lives, so
//! it can't be constructed anywhere else, and existential lifetimes - a
//! lifetime guaranteeing T exists, granting no access, held by the container
//! - so the drop ordering is the compiler's to check.

use core::ops::Deref;
use core::ptr::NonNull;

pub struct BackRef<T: ?Sized + Sync>(NonNull<T>);

impl<T: ?Sized + Sync> BackRef<T> {
    /// # Safety
    /// @t outlives this BackRef, and what's handed a reference from it - its
    /// owner drains what holds one, and what that holds, before it goes - and
    /// is only accessed through shared references meanwhile.
    pub unsafe fn new(t: &T) -> Self {
        BackRef(NonNull::from(t))
    }

    /// # Safety
    /// What's handed it is drained with this BackRef's holder, before @t goes.
    pub unsafe fn get(&self) -> &'static T
    where
        T: 'static,
    {
        // SAFETY: new()'s, and the caller's
        unsafe { self.0.as_ref() }
    }
}

impl<T: ?Sized + Sync> Deref for BackRef<T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: new()'s
        unsafe { self.0.as_ref() }
    }
}

// SAFETY: a &T for as long as it lives, by new()'s contract - and a &T is
// Send and Sync when T is Sync
unsafe impl<T: ?Sized + Sync> Send for BackRef<T> {}
unsafe impl<T: ?Sized + Sync> Sync for BackRef<T> {}
