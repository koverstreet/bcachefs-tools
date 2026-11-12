// SPDX-License-Identifier: GPL-2.0

//! A spinlock_t taken with spin_lock(), interrupts as they are, for what no
//! interrupt takes - where raw.rs's spinlock_t is taken irqsave. Not
//! raw_spinlock_t: on PREEMPT_RT this one sleeps, so it can be taken in an RCU
//! read section with a GFP_NOWAIT allocation under it, where RT forbids
//! allocating under a raw_spinlock_t.

use super::raw::{rust_spin_lock_assert_held, rust_spin_lock_init};
#[cfg(not(kernel))]
use super::raw::{rust_raw_spin_lock, rust_raw_spin_unlock};
use super::{MemallocFlags, RawLock, RawLockBase, RawMovableLock};
use super::spinlock_t;
use core::convert::Infallible;
use core::ffi::{c_void, CStr};

#[cfg(kernel)]
use kernel::bindings as b;

#[repr(transparent)]
pub struct SpinLock(spinlock_t);

// SAFETY: zeroed as spinlock_t is: a lock only once init() runs
unsafe impl ::pin_init::Zeroable for SpinLock {}

unsafe impl RawLockBase for SpinLock {
    type GuardState = MemallocFlags;
    type InitError = Infallible;

    unsafe fn init(l: *mut Self, name: &'static CStr, key: *mut c_void) -> Result<(), Infallible> {
        unsafe { rust_spin_lock_init(l.cast(), name.as_ptr().cast(), key) };
        Ok(())
    }
}

unsafe impl RawMovableLock for SpinLock {}

unsafe impl RawLock for SpinLock {
    unsafe fn lock(&self) -> MemallocFlags {
        #[cfg(kernel)]
        unsafe { b::spin_lock(self.0.as_ptr()) };
        // the shim's spinlock_t is its raw_spinlock_t
        #[cfg(not(kernel))]
        unsafe { rust_raw_spin_lock(self.0.as_ptr().cast()) };
        MemallocFlags::noreclaim()
    }

    unsafe fn unlock(&self, _noreclaim: MemallocFlags) {
        #[cfg(kernel)]
        unsafe { b::spin_unlock(self.0.as_ptr()) };
        #[cfg(not(kernel))]
        unsafe { rust_raw_spin_unlock(self.0.as_ptr().cast()) };
    }

    unsafe fn assert_held(&self) {
        unsafe { rust_spin_lock_assert_held(self.0.as_ptr().cast()) }
    }
}
