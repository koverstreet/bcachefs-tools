// SPDX-License-Identifier: GPL-2.0

use core::ops::Deref;
use core::sync::atomic::{AtomicU32, Ordering::{Acquire, Relaxed, Release}};

use super::{Refcount, TryRefcount, WaitableRef};
use crate::util::futex::{futex_wait, futex_wake};
use crate::util::sleep::Sleep;

/// A count and its value, as an Arc's, but not owning its memory: it starts
/// at one, the owner's ref, and the owner frees it after wait(). For slow
/// paths: PercpuRef and EnumeratedRef are percpu.
///
/// wait() sleeps on the count itself: the write that drops it to STOPPED is
/// the last a put makes, and the wake after it only hashes the address - so
/// wait() returning, and the owner freeing it, can't race a put still
/// touching it.
#[repr(C)]
pub struct AtomicRef<T> {
    count: AtomicU32,
    value: T,
}

const STOPPED: u32 = 1 << 31;

impl<T> AtomicRef<T> {
    pub const fn new(value: T) -> Self {
        AtomicRef { count: AtomicU32::new(1), value }
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
            self.wake();
        }
    }
}

unsafe impl<T> TryRefcount for AtomicRef<T> {
    fn try_inc(&self) -> bool {
        self.count.fetch_update(Relaxed, Relaxed, |r| (r & STOPPED == 0).then_some(r + 1)).is_ok()
    }
}

impl<T> AtomicRef<T> {
    fn wake(&self) {
        futex_wake(core::ptr::from_ref(&self.count).cast(), usize::MAX);
    }
}

impl<T> WaitableRef for AtomicRef<T> {
    fn stop(&self) {
        if self.count.fetch_update(Release, Relaxed, |r| (r & STOPPED == 0).then_some((r - 1) | STOPPED))
               == Ok(1) {
            self.wake();
        }
    }

    fn wait(&self) {
        loop {
            let r = self.count.load(Acquire);
            if r == STOPPED {
                return;
            }
            let Ok(()) = futex_wait(&self.count, r, Sleep::UNINTERRUPTIBLE);
        }
    }
}
