// SPDX-License-Identifier: GPL-2.0

//! The kernel's spinlocks, as RawLocks: spinlock_t taken irqsave, SpinLock -
//! a spinlock_t taken with spin_lock() - and raw_spinlock_t. Initializing in
//! a lockdep class and spin_lock_irqsave() are macros: shim.c's rust_*
//! functions.

#![allow(non_camel_case_types)]

use crate::c;
use crate::locking::{MemallocFlags, RawLock, RawLockBase, RawMovableLock, RawNestedLock};
use crate::os::lockdep;
use crate::util::ffi::Opaque;
use core::convert::Infallible;
use core::ffi::{c_ulong, c_void, CStr};
use kernel::bindings as b;

pub type spinlock_t = Opaque<b::spinlock_t>;
pub type raw_spinlock_t = Opaque<b::raw_spinlock_t>;

// Zeroed as the structs holding them start; a lock only once their init runs
unsafe impl ::pin_init::Zeroable for spinlock_t {}
unsafe impl ::pin_init::Zeroable for raw_spinlock_t {}

// ── spinlock_t, taken irqsave ────────────────────────────────────────────

// Taken with interrupts off - spin_lock_irqsave() - for what interrupt
// context takes too, an endio; held, it mustn't sleep. The kernel crate's
// SpinLock leaves interrupts on.
unsafe impl RawLockBase for spinlock_t {
    type GuardState = (c_ulong, MemallocFlags);
    type InitError = Infallible;

    unsafe fn init(l: *mut Self, name: &'static CStr, key: *mut c_void) -> Result<(), Infallible> {
        unsafe { c::rust_spin_lock_init(l.cast(), name.as_ptr().cast(), key) };
        Ok(())
    }
}

// unlocked, nothing points at one
unsafe impl RawMovableLock for spinlock_t {}

unsafe impl RawLock for spinlock_t {
    unsafe fn lock(&self) -> (c_ulong, MemallocFlags) {
        let flags = unsafe { c::rust_spin_lock_irqsave(self.as_ptr().cast()) };
        (flags, MemallocFlags::noreclaim())
    }

    unsafe fn unlock(&self, (flags, _noreclaim): (c_ulong, MemallocFlags)) {
        unsafe { c::rust_spin_unlock_irqrestore(self.as_ptr().cast(), flags) }
    }

    unsafe fn assert_held(&self) {
        lockdep::spin_lock_assert_held(self)
    }
}

unsafe impl RawNestedLock for spinlock_t {
    unsafe fn lock_nested(&self, subclass: u32) -> (c_ulong, MemallocFlags) {
        let flags = unsafe { c::rust_spin_lock_irqsave_nested(self.as_ptr().cast(), subclass) };
        (flags, MemallocFlags::noreclaim())
    }
}

// ── SpinLock: spinlock_t, taken with spin_lock() ─────────────────────────

/// A spinlock_t taken with spin_lock(), interrupts as they are, for what no
/// interrupt takes - where spinlock_t is taken irqsave. Not raw_spinlock_t:
/// on PREEMPT_RT this one sleeps, so it can be taken in an RCU read section
/// with a GFP_NOWAIT allocation under it, where RT forbids allocating under a
/// raw_spinlock_t.
#[repr(transparent)]
pub struct SpinLock(spinlock_t);

// SAFETY: zeroed as spinlock_t is: a lock only once init() runs
unsafe impl ::pin_init::Zeroable for SpinLock {}

unsafe impl RawLockBase for SpinLock {
    type GuardState = MemallocFlags;
    type InitError = Infallible;

    unsafe fn init(l: *mut Self, name: &'static CStr, key: *mut c_void) -> Result<(), Infallible> {
        unsafe { c::rust_spin_lock_init(l.cast(), name.as_ptr().cast(), key) };
        Ok(())
    }
}

unsafe impl RawMovableLock for SpinLock {}

unsafe impl RawLock for SpinLock {
    unsafe fn lock(&self) -> MemallocFlags {
        unsafe { b::spin_lock(self.0.as_ptr()) };
        MemallocFlags::noreclaim()
    }

    unsafe fn unlock(&self, _noreclaim: MemallocFlags) {
        unsafe { b::spin_unlock(self.0.as_ptr()) };
    }

    unsafe fn assert_held(&self) {
        lockdep::spin_lock_assert_held(&self.0)
    }
}

// ── raw_spinlock_t ───────────────────────────────────────────────────────

// Taken with interrupts as they are, for what no interrupt takes - a lock's
// wait list. A spinning lock on PREEMPT_RT too, where spinlock_t sleeps.
unsafe impl RawLockBase for raw_spinlock_t {
    type GuardState = MemallocFlags;
    type InitError = Infallible;

    unsafe fn init(l: *mut Self, name: &'static CStr, key: *mut c_void) -> Result<(), Infallible> {
        unsafe { c::rust_raw_spin_lock_init(l.cast(), name.as_ptr().cast(), key) };
        Ok(())
    }
}

unsafe impl RawMovableLock for raw_spinlock_t {}

unsafe impl RawLock for raw_spinlock_t {
    unsafe fn lock(&self) -> MemallocFlags {
        unsafe { c::rust_raw_spin_lock(self.as_ptr().cast()) };
        MemallocFlags::noreclaim()
    }

    unsafe fn unlock(&self, _noreclaim: MemallocFlags) {
        unsafe { c::rust_raw_spin_unlock(self.as_ptr().cast()) }
    }

    unsafe fn assert_held(&self) {
        lockdep::raw_spin_lock_assert_held(self)
    }
}
