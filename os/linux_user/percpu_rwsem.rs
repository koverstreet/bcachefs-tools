// SPDX-License-Identifier: GPL-2.0

//! PercpuRwSem, for userspace: parking_lot's RawRwLock, as RwSem is - readers
//! share it, as the kernel's do; there are no CPUs' counts to allocate, so
//! init can't fail.

use crate::errcode::BchError;
use crate::locking::{RawLock, RawLockBase, RawReadLock, RawRwLock};
use crate::os::lockdep::LockClass;
use core::ffi::CStr;
use parking_lot::lock_api::RawRwLock as _;

#[repr(transparent)]
pub struct PercpuRwSem(parking_lot::RawRwLock);

unsafe impl RawLockBase for PercpuRwSem {
    type GuardState = ();
    type InitError = BchError;

    unsafe fn init(l: *mut Self, _name: &'static CStr, _key: LockClass) -> Result<(), BchError> {
        unsafe { l.write(PercpuRwSem(parking_lot::RawRwLock::INIT)) };
        Ok(())
    }
}

unsafe impl RawLock for PercpuRwSem {
    fn lock(&self) {
        self.0.lock_exclusive()
    }

    unsafe fn unlock(&self, _: ()) {
        unsafe { self.0.unlock_exclusive() }
    }

    fn assert_held(&self) {}
}

unsafe impl RawReadLock for PercpuRwSem {
    fn lock_read(&self) {
        self.0.lock_shared()
    }

    unsafe fn unlock_read(&self, _: ()) {
        unsafe { self.0.unlock_shared() }
    }
}

unsafe impl RawRwLock for PercpuRwSem {
    fn assert_held_any(&self) {}
}
