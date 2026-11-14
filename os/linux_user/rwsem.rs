// SPDX-License-Identifier: GPL-2.0

//! rw_semaphore, for userspace, as a RawRwLock: parking_lot's - one word, so
//! an unlocked one may be moved, where a copy of a pthread rwlock isn't one.

#![allow(non_camel_case_types)]

use crate::locking::{RawLock, RawLockBase, RawReadLock, RawRwLock, RawTryReadLock};
use core::convert::Infallible;
use core::ffi::{c_void, CStr};
use parking_lot::lock_api::RawRwLock as _;

#[repr(transparent)]
pub struct rw_semaphore(parking_lot::RawRwLock);

unsafe impl ::pin_init::Zeroable for rw_semaphore {}

unsafe impl RawLockBase for rw_semaphore {
    type GuardState = ();
    type InitError = Infallible;

    unsafe fn init(l: *mut Self, _name: &'static CStr, _key: *mut c_void) -> Result<(), Infallible> {
        unsafe { l.write(rw_semaphore(parking_lot::RawRwLock::INIT)) };
        Ok(())
    }
}

unsafe impl RawLock for rw_semaphore {
    unsafe fn lock(&self) {
        self.0.lock_exclusive()
    }

    unsafe fn unlock(&self, _: ()) {
        unsafe { self.0.unlock_exclusive() }
    }

    unsafe fn assert_held(&self) {}
}

unsafe impl RawReadLock for rw_semaphore {
    unsafe fn lock_read(&self) {
        self.0.lock_shared()
    }

    unsafe fn unlock_read(&self, _: ()) {
        unsafe { self.0.unlock_shared() }
    }
}

unsafe impl RawTryReadLock for rw_semaphore {
    unsafe fn try_lock_read(&self) -> Option<()> {
        self.0.try_lock_shared().then_some(())
    }
}

unsafe impl RawRwLock for rw_semaphore {
    unsafe fn assert_held_any(&self) {}
}
