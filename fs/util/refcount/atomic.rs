// SPDX-License-Identifier: GPL-2.0

use core::cell::UnsafeCell;
use core::mem::ManuallyDrop;
use core::sync::atomic::{fence, AtomicU32, Ordering::{Acquire, Relaxed, Release}};

use pin_init::{pin_init_from_closure, PinInit};

use super::{RefGuard, Refcount, TryRefcount, WaitableRef};
use crate::util::futex::{futex_wait, futex_wake};
use crate::util::place::Placed;
use crate::util::sleep::Sleep;

/// A count and its value, as an Arc's, but not owning its memory: it starts
/// at one, the owner's ref. The value is had only through a ref, and the put
/// that leaves none, once it's stopped, drops it; the owner frees the memory
/// after wait(). For slow paths: PercpuRef and EnumeratedRef are percpu.
///
/// wait() sleeps on the count itself: the store of RELEASED, after the value's
/// dropped, is the last write a put makes, and the wake after it only hashes
/// the address - so wait() returning, and the owner freeing it, can't race a
/// put still touching it.
#[repr(C)]
pub struct AtomicRef<T> {
    count: AtomicU32,
    value: UnsafeCell<ManuallyDrop<T>>,
}

const STOPPED: u32 = 1 << 31;
const RELEASED: u32 = 1 << 30;

unsafe impl<T: Send + Sync> Send for AtomicRef<T> {}
unsafe impl<T: Send + Sync> Sync for AtomicRef<T> {}

impl<T> AtomicRef<T> {
    pub const fn new(value: T) -> Self {
        AtomicRef { count: AtomicU32::new(1), value: UnsafeCell::new(ManuallyDrop::new(value)) }
    }

    /// new(), for a value that's pinned.
    pub fn pin_init<E>(value: impl PinInit<T, E>) -> impl PinInit<Self, E> {
        unsafe {
            pin_init_from_closure(move |slot: *mut Self| {
                (&raw mut (*slot).count).write(AtomicU32::new(1));
                crate::util::alloc::init_at(value, (&raw mut (*slot).value).cast::<T>())
            })
        }
    }

    /// # Safety
    /// A ref is held for as long as the reference lives.
    pub(crate) unsafe fn value_held(&self) -> &T {
        unsafe { &*self.value.get() }
    }

    /// By the put that left no refs, once stopped.
    #[cold]
    #[inline(never)]
    fn release(&self) {
        fence(Acquire);
        unsafe { ManuallyDrop::drop(&mut *self.value.get()) };
        self.count.store(STOPPED | RELEASED, Release);
        futex_wake(core::ptr::from_ref(&self.count).cast(), usize::MAX);
    }
}

impl<T> Drop for AtomicRef<T> {
    fn drop(&mut self) {
        let r = *self.count.get_mut();
        if r & RELEASED == 0 {
            assert!(r == 1, "AtomicRef dropped with refs held: count {r:#x}");
            unsafe { ManuallyDrop::drop(self.value.get_mut()) }
        }
    }
}

impl<T> RefGuard<AtomicRef<T>> {
    pub fn value(&self) -> &T {
        unsafe { (**self).value_held() }
    }
}

impl<T, A> RefGuard<Placed<AtomicRef<T>, A>> {
    pub fn value(&self) -> &T {
        unsafe { (**self).value_held() }
    }
}

unsafe impl<T> Refcount for AtomicRef<T> {
    unsafe fn inc(&self) {
        self.count.fetch_add(1, Relaxed);
    }

    unsafe fn dec(&self) {
        if self.count.fetch_sub(1, Release) == STOPPED | 1 {
            self.release();
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
            self.release();
        }
    }

    fn wait(&self) {
        loop {
            let r = self.count.load(Acquire);
            if r == STOPPED | RELEASED {
                return;
            }
            let Ok(()) = futex_wait(&self.count, r, Sleep::UNINTERRUPTIBLE);
        }
    }
}
