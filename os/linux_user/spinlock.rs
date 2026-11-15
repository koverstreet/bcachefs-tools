// SPDX-License-Identifier: GPL-2.0

//! Spinlocks, for userspace: a futex word, not a pthread mutex - an unlocked
//! one may be moved, as a kernel spinlock may, where a copy of a pthread mutex
//! isn't one. SpinLockIrq, SpinLock and RawSpinLock are the same lock here:
//! there are no interrupts to save, and nothing sleeps that a RawSpinLock
//! wouldn't. They're kept apart for what each may do, as the kernel's are.

use crate::locking::{MemallocFlags, RawLock, RawLockBase, RawNestedLock};
use crate::os::lockdep::LockClass;
use core::convert::Infallible;
use core::ffi::CStr;
use core::sync::atomic::{AtomicU32, Ordering::{Acquire, Relaxed, Release}};
use rustix::thread::futex;

const UNLOCKED: u32 = 0;
const LOCKED: u32 = 1;
/* locked, and someone may be waiting: the unlock wakes them */
const CONTENDED: u32 = 2;

#[repr(C)]
pub struct RawSpinLock {
    state: AtomicU32,
}

#[repr(transparent)]
pub struct SpinLockIrq(RawSpinLock);

#[repr(transparent)]
pub struct SpinLock(RawSpinLock);

// ── RawSpinLock ──────────────────────────────────────────────────────────

impl RawLockBase for RawSpinLock {
    type GuardState = MemallocFlags;
    type InitError = Infallible;

    unsafe fn init(l: *mut Self, _name: &'static CStr, _key: LockClass) -> Result<(), Infallible> {
        unsafe { (&raw mut (*l).state).write(AtomicU32::new(UNLOCKED)) };
        Ok(())
    }
}

impl RawLock for RawSpinLock {
    fn lock(&self) -> MemallocFlags {
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

    fn assert_held(&self) {}
}

// ── SpinLockIrq ──────────────────────────────────────────────────────────

impl RawLockBase for SpinLockIrq {
    type GuardState = MemallocFlags;
    type InitError = Infallible;

    unsafe fn init(l: *mut Self, name: &'static CStr, key: LockClass) -> Result<(), Infallible> {
        unsafe { RawSpinLock::init(l.cast(), name, key) }
    }
}

impl RawLock for SpinLockIrq {
    fn lock(&self) -> MemallocFlags {
        self.0.lock()
    }

    unsafe fn unlock(&self, noreclaim: MemallocFlags) {
        unsafe { self.0.unlock(noreclaim) }
    }

    fn assert_held(&self) {}
}

impl RawNestedLock for SpinLockIrq {
    fn lock_nested(&self, _subclass: u32) -> MemallocFlags {
        self.lock()
    }
}

// ── SpinLock ─────────────────────────────────────────────────────────────

impl RawLockBase for SpinLock {
    type GuardState = MemallocFlags;
    type InitError = Infallible;

    unsafe fn init(l: *mut Self, name: &'static CStr, key: LockClass) -> Result<(), Infallible> {
        unsafe { RawSpinLock::init(l.cast(), name, key) }
    }
}

impl RawLock for SpinLock {
    fn lock(&self) -> MemallocFlags {
        self.0.lock()
    }

    unsafe fn unlock(&self, noreclaim: MemallocFlags) {
        unsafe { self.0.unlock(noreclaim) }
    }

    fn assert_held(&self) {}
}
