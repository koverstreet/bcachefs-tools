// SPDX-License-Identifier: GPL-2.0
//! Userspace stand-in for the subset of `kernel::sync` that fs/ uses, with
//! the same names and semantics.

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicU32, Ordering::{Acquire, Relaxed, Release}};

/// kernel::sync::SetOnce: populated at most once, then read by anyone.
///
/// # Invariants
/// @init only goes up, 0 to 2: 0, @value uninitialized; 1, one populate()
/// writing it; 2, initialized, and shared.
pub struct SetOnce<T> {
    init:  AtomicU32,
    value: UnsafeCell<MaybeUninit<T>>,
}

impl<T> Default for SetOnce<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> SetOnce<T> {
    pub const fn new() -> Self {
        Self { init: AtomicU32::new(0), value: UnsafeCell::new(MaybeUninit::uninit()) }
    }

    pub fn as_ref(&self) -> Option<&T> {
        // SAFETY: 2 - initialized, and shared
        (self.init.load(Acquire) == 2).then(|| unsafe { (*self.value.get()).assume_init_ref() })
    }

    /// Whether this populated it: false, @value dropped, if it was already.
    pub fn populate(&self, value: T) -> bool {
        if self.init.compare_exchange(0, 1, Relaxed, Relaxed).is_err() {
            return false;
        }
        // SAFETY: 1 - this is the one writer
        unsafe { (*self.value.get()).write(value) };
        self.init.store(2, Release);
        true
    }
}

impl<T> Drop for SetOnce<T> {
    fn drop(&mut self) {
        if *self.init.get_mut() == 2 {
            // SAFETY: initialized, and ours
            unsafe { self.value.get_mut().assume_init_drop() };
        }
    }
}

// SAFETY: as the kernel's - its T's
unsafe impl<T: Send> Send for SetOnce<T> {}
unsafe impl<T: Send + Sync> Sync for SetOnce<T> {}
