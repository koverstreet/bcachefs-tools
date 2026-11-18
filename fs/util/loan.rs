// SPDX-License-Identifier: GPL-2.0

//! Loan<T>: a &T lent for the length of a call, for others to use meanwhile,
//! under the loan's lock - a filesystem lent to its status channel while it
//! starts, say, which outlives it.
//!
//! lend() is closure-scoped, not a guard: a guard could be leaked, and the
//! lent pointer left behind it. The loan's over - cleared under the lock, so
//! no with() is still using it - before lend() returns.

use core::ptr::NonNull;

use pin_init::{pin_data, pin_init, PinInit};

use crate::locking::{Locked, Mutex};
use crate::new_lock;

#[pin_data]
pub struct Loan<T> {
    #[pin]
    lent: Locked<Mutex, Option<NonNull<T>>>,
}

// what's lent is a &T, used from whatever thread asks
unsafe impl<T: Sync> Send for Loan<T> {}
unsafe impl<T: Sync> Sync for Loan<T> {}

impl<T> Loan<T> {
    pub fn new() -> impl PinInit<Self> {
        pin_init!(Loan { lent <- new_lock!(None) })
    }

    /// @f, with @t lent: with() has it till @f returns.
    pub fn lend<R>(&self, t: &T, f: impl FnOnce() -> R) -> R {
        struct Over<'a, T>(&'a Locked<Mutex, Option<NonNull<T>>>);

        impl<T> Drop for Over<'_, T> {
            fn drop(&mut self) {
                *self.0.lock() = None;
            }
        }

        *self.lent.lock() = Some(NonNull::from(t));
        let _over = Over(&self.lent);
        f()
    }

    /// @f, of what's lent, if anything is: under the lock, which the loan's
    /// end waits on.
    pub fn with<R>(&self, f: impl FnOnce(Option<&T>) -> R) -> R {
        let lent = self.lent.lock();
        f(lent.map(|p| unsafe { p.as_ref() }))
    }
}
