// SPDX-License-Identifier: GPL-2.0

use core::ops::Deref;
use core::sync::atomic::{AtomicU32, Ordering::{Acquire, Relaxed, Release}};

use super::{Refcount, TryRefcount, WaitableRef};
use crate::util::futex::EventCount;
use crate::util::sleep::Sleep;

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
