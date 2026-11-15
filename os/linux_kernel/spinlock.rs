// SPDX-License-Identifier: GPL-2.0

//! The kernel's spinlocks, as RawLocks: SpinLockIrq, a spinlock_t taken
//! irqsave; SpinLock, a spinlock_t taken with spin_lock(); and RawSpinLock, a
//! raw_spinlock_t. Initializing in a lockdep class and spin_lock_irqsave() are
//! macros: shim.c's rust_* functions.

use crate::c;
use crate::locking::{MemallocFlags, RawLock, RawLockBase, RawMovableLock, RawNestedLock};
use crate::os::lockdep::{self, LockClass};
use crate::util::ffi::Opaque;
use core::convert::Infallible;
use core::ffi::{c_ulong, CStr};
use kernel::bindings as b;

pub type SpinLockIrq = Opaque<b::spinlock_t>;
pub type RawSpinLock = Opaque<b::raw_spinlock_t>;

// taken and released from any thread
unsafe impl Send for SpinLockIrq {}
unsafe impl Sync for SpinLockIrq {}
unsafe impl Send for RawSpinLock {}
unsafe impl Sync for RawSpinLock {}

// ── SpinLockIrq: spinlock_t, taken irqsave ───────────────────────────────

// Taken with interrupts off - spin_lock_irqsave() - for what interrupt
// context takes too, an endio; held, it mustn't sleep.
impl RawLockBase for SpinLockIrq {
    type GuardState = (c_ulong, MemallocFlags);
    type InitError = Infallible;

    unsafe fn init(l: *mut Self, name: &'static CStr, key: LockClass) -> Result<(), Infallible> {
        unsafe { c::rust_spin_lock_init(l.cast(), name.as_ptr().cast(), key.as_ptr().cast()) };
        Ok(())
    }
}

// unlocked, nothing points at one
impl RawMovableLock for SpinLockIrq {}

impl RawLock for SpinLockIrq {
    fn lock(&self) -> (c_ulong, MemallocFlags) {
        let flags = unsafe { c::rust_spin_lock_irqsave(self.as_ptr().cast()) };
        (flags, MemallocFlags::noreclaim())
    }

    unsafe fn unlock(&self, (flags, _noreclaim): (c_ulong, MemallocFlags)) {
        unsafe { c::rust_spin_unlock_irqrestore(self.as_ptr().cast(), flags) }
    }

    fn assert_held(&self) {
        lockdep::spin_lock_assert_held(self)
    }
}

impl RawNestedLock for SpinLockIrq {
    fn lock_nested(&self, subclass: u32) -> (c_ulong, MemallocFlags) {
        let flags = unsafe { c::rust_spin_lock_irqsave_nested(self.as_ptr().cast(), subclass) };
        (flags, MemallocFlags::noreclaim())
    }
}

// ── SpinLock: spinlock_t, taken with spin_lock() ─────────────────────────

/// A spinlock_t taken with spin_lock(), interrupts as they are, for what no
/// interrupt takes - where SpinLockIrq is taken irqsave. Not RawSpinLock: on
/// PREEMPT_RT this one sleeps, so it can be taken in an RCU read section with
/// a GFP_NOWAIT allocation under it, where RT forbids allocating under a
/// raw_spinlock_t.
#[repr(transparent)]
pub struct SpinLock(Opaque<b::spinlock_t>);

unsafe impl Send for SpinLock {}
unsafe impl Sync for SpinLock {}

impl RawLockBase for SpinLock {
    type GuardState = MemallocFlags;
    type InitError = Infallible;

    unsafe fn init(l: *mut Self, name: &'static CStr, key: LockClass) -> Result<(), Infallible> {
        unsafe { c::rust_spin_lock_init(l.cast(), name.as_ptr().cast(), key.as_ptr().cast()) };
        Ok(())
    }
}

impl RawMovableLock for SpinLock {}

impl RawLock for SpinLock {
    fn lock(&self) -> MemallocFlags {
        unsafe { b::spin_lock(self.0.as_ptr()) };
        MemallocFlags::noreclaim()
    }

    unsafe fn unlock(&self, _noreclaim: MemallocFlags) {
        unsafe { b::spin_unlock(self.0.as_ptr()) };
    }

    fn assert_held(&self) {
        lockdep::spin_lock_assert_held(&self.0)
    }
}

// ── RawSpinLock: raw_spinlock_t ──────────────────────────────────────────

// Taken with interrupts as they are, for what no interrupt takes - a lock's
// wait list. A spinning lock on PREEMPT_RT too, where spinlock_t sleeps.
impl RawLockBase for RawSpinLock {
    type GuardState = MemallocFlags;
    type InitError = Infallible;

    unsafe fn init(l: *mut Self, name: &'static CStr, key: LockClass) -> Result<(), Infallible> {
        unsafe { c::rust_raw_spin_lock_init(l.cast(), name.as_ptr().cast(), key.as_ptr().cast()) };
        Ok(())
    }
}

impl RawMovableLock for RawSpinLock {}

impl RawLock for RawSpinLock {
    fn lock(&self) -> MemallocFlags {
        unsafe { c::rust_raw_spin_lock(self.as_ptr().cast()) };
        MemallocFlags::noreclaim()
    }

    unsafe fn unlock(&self, _noreclaim: MemallocFlags) {
        unsafe { c::rust_raw_spin_unlock(self.as_ptr().cast()) }
    }

    fn assert_held(&self) {
        lockdep::raw_spin_lock_assert_held(self)
    }
}
