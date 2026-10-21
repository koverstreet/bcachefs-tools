// SPDX-License-Identifier: GPL-2.0
//! Userspace stand-in for the subset of `kernel::sync` that fs/ uses, with the
//! same names, so fs/ code is written once against the kernel's API.
//!
//! The kernel's Mutex is pinned and built in place from a PinInit, because a
//! struct mutex can't move; std's can, so here new_mutex!() is just the value -
//! which pin-init takes as an initializer of itself: code that initializes in
//! place - a Rust member of a C struct - is the same in both builds.

use core::ops::{Deref, DerefMut};

/// A mutex, as the kernel's: lock() can't fail. A panic while it was held
/// doesn't poison it, as there's nothing in-kernel to poison.
pub struct Mutex<T>(std::sync::Mutex<T>);

/// A held Mutex: as the kernel's Guard.
pub struct MutexGuard<'a, T>(std::sync::MutexGuard<'a, T>);

impl<T> Mutex<T> {
    /// What new_mutex!() expands to; use that, as in-kernel.
    pub const fn new(v: T) -> Self {
        Mutex(std::sync::Mutex::new(v))
    }

    pub fn lock(&self) -> MutexGuard<'_, T> {
        MutexGuard(self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
    }
}

impl<T> Deref for MutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

/// A Mutex initializer for @inner: as the kernel's new_mutex!(), which takes
/// an optional name (for lockdep) that userspace has no use for.
#[macro_export]
macro_rules! new_mutex {
    ($inner:expr $(, $name:literal)? $(,)?) => {
        $crate::sync::Mutex::new($inner)
    };
}
pub use new_mutex;
