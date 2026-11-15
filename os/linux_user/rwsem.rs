// SPDX-License-Identifier: GPL-2.0

//! RwSem, for userspace: parking_lot's RawRwLock - one word, so an unlocked
//! one may be moved, where a copy of a pthread rwlock isn't one.

use crate::locking::{RawLock, RawLockBase, RawReadLock, RawRwLock, RawTryReadLock};
use crate::os::lockdep::LockClass;
use core::convert::Infallible;
use core::ffi::CStr;
use parking_lot::lock_api::RawRwLock as _;

#[repr(transparent)]
pub struct RwSem(parking_lot::RawRwLock);

unsafe impl RawLockBase for RwSem {
    type GuardState = ();
    type InitError = Infallible;

    unsafe fn init(l: *mut Self, _name: &'static CStr, _key: LockClass) -> Result<(), Infallible> {
        unsafe { l.write(RwSem(parking_lot::RawRwLock::INIT)) };
        Ok(())
    }
}

unsafe impl RawLock for RwSem {
    unsafe fn lock(&self) {
        self.0.lock_exclusive()
    }

    unsafe fn unlock(&self, _: ()) {
        unsafe { self.0.unlock_exclusive() }
    }

    unsafe fn assert_held(&self) {}
}

unsafe impl RawReadLock for RwSem {
    unsafe fn lock_read(&self) {
        self.0.lock_shared()
    }

    unsafe fn unlock_read(&self, _: ()) {
        unsafe { self.0.unlock_shared() }
    }
}

unsafe impl RawTryReadLock for RwSem {
    unsafe fn try_lock_read(&self) -> Option<()> {
        self.0.try_lock_shared().then_some(())
    }
}

unsafe impl RawRwLock for RwSem {
    unsafe fn assert_held_any(&self) {}
}
