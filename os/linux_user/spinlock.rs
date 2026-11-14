// SPDX-License-Identifier: GPL-2.0

//! Spinlocks, for userspace, as RawLocks: a futex word, not a pthread mutex -
//! an unlocked one may be moved, as a kernel spinlock may, where a copy of a
//! pthread mutex isn't one. spinlock_t, SpinLock and raw_spinlock_t are the
//! same lock here: there are no interrupts to save, and nothing sleeps that
//! a raw_spinlock_t wouldn't.

#![allow(non_camel_case_types)]

use crate::locking::{MemallocFlags, RawLock, RawLockBase, RawMovableLock, RawNestedLock};
use core::convert::Infallible;
use core::ffi::{c_ulong, c_void, CStr};
use core::sync::atomic::{AtomicU32, Ordering::{Acquire, Relaxed, Release}};
use rustix::thread::futex;

const UNLOCKED: u32 = 0;
const LOCKED: u32 = 1;
/* locked, and someone may be waiting: the unlock wakes them */
const CONTENDED: u32 = 2;

#[repr(C)]
pub struct raw_spinlock_t {
    state: AtomicU32,
}

#[repr(transparent)]
pub struct spinlock_t(raw_spinlock_t);

#[repr(transparent)]
pub struct SpinLock(spinlock_t);

// Zeroed as the structs holding them start: unlocked
unsafe impl ::pin_init::Zeroable for raw_spinlock_t {}
unsafe impl ::pin_init::Zeroable for spinlock_t {}
unsafe impl ::pin_init::Zeroable for SpinLock {}

// ── raw_spinlock_t ───────────────────────────────────────────────────────

unsafe impl RawLockBase for raw_spinlock_t {
    type GuardState = MemallocFlags;
    type InitError = Infallible;

    unsafe fn init(l: *mut Self, _name: &'static CStr, _key: *mut c_void) -> Result<(), Infallible> {
        unsafe { (&raw mut (*l).state).write(AtomicU32::new(UNLOCKED)) };
        Ok(())
    }
}

unsafe impl RawMovableLock for raw_spinlock_t {}

unsafe impl RawLock for raw_spinlock_t {
    unsafe fn lock(&self) -> MemallocFlags {
        let s = &self.state;
        if s.compare_exchange(UNLOCKED, LOCKED, Acquire, Relaxed).is_err() {
            while s.swap(CONTENDED, Acquire) != UNLOCKED {
                let _ = futex::wait(s, futex::Flags::PRIVATE, CONTENDED, None);
            }
        }
        MemallocFlags::noreclaim()
    }

    unsafe fn unlock(&self, _noreclaim: MemallocFlags) {
        let s = &self.state;
        if s.swap(UNLOCKED, Release) == CONTENDED {
            let _ = futex::wake(s, futex::Flags::PRIVATE, 1);
        }
    }

    unsafe fn assert_held(&self) {}
}

// ── spinlock_t, with the kernel's irqsave guard state: no flags here ──────

unsafe impl RawLockBase for spinlock_t {
    type GuardState = (c_ulong, MemallocFlags);
    type InitError = Infallible;

    unsafe fn init(l: *mut Self, name: &'static CStr, key: *mut c_void) -> Result<(), Infallible> {
        unsafe { raw_spinlock_t::init(l.cast(), name, key) }
    }
}

unsafe impl RawMovableLock for spinlock_t {}

unsafe impl RawLock for spinlock_t {
    unsafe fn lock(&self) -> (c_ulong, MemallocFlags) {
        (0, unsafe { self.0.lock() })
    }

    unsafe fn unlock(&self, (_flags, noreclaim): (c_ulong, MemallocFlags)) {
        unsafe { self.0.unlock(noreclaim) }
    }

    unsafe fn assert_held(&self) {}
}

unsafe impl RawNestedLock for spinlock_t {
    unsafe fn lock_nested(&self, _subclass: u32) -> (c_ulong, MemallocFlags) {
        unsafe { self.lock() }
    }
}

// ── SpinLock ─────────────────────────────────────────────────────────────

unsafe impl RawLockBase for SpinLock {
    type GuardState = MemallocFlags;
    type InitError = Infallible;

    unsafe fn init(l: *mut Self, name: &'static CStr, key: *mut c_void) -> Result<(), Infallible> {
        unsafe { raw_spinlock_t::init(l.cast(), name, key) }
    }
}

unsafe impl RawMovableLock for SpinLock {}

unsafe impl RawLock for SpinLock {
    unsafe fn lock(&self) -> MemallocFlags {
        unsafe { self.0.0.lock() }
    }

    unsafe fn unlock(&self, noreclaim: MemallocFlags) {
        unsafe { self.0.0.unlock(noreclaim) }
    }

    unsafe fn assert_held(&self) {}
}
