// SPDX-License-Identifier: GPL-2.0

//! Mutex, for userspace: parking_lot's - one byte, so an unlocked one may be
//! moved, where a copy of a pthread mutex isn't one.

use crate::locking::{RawLock, RawLockBase, RawNestedLock, RawTryLock};
use crate::os::lockdep::LockClass;
use core::convert::Infallible;
use core::ffi::CStr;
use parking_lot::lock_api::RawMutex as _;

#[repr(transparent)]
pub struct Mutex(parking_lot::RawMutex);

impl RawLockBase for Mutex {
    type GuardState = ();
    type InitError = Infallible;

    unsafe fn init(l: *mut Self, _name: &'static CStr, _key: LockClass) -> Result<(), Infallible> {
        unsafe { l.write(Mutex(parking_lot::RawMutex::INIT)) };
        Ok(())
    }
}

impl RawLock for Mutex {
    fn lock(&self) {
        self.0.lock()
    }

    unsafe fn unlock(&self, _: ()) {
        unsafe { self.0.unlock() }
    }

    fn assert_held(&self) {}
}

impl RawTryLock for Mutex {
    fn try_lock(&self) -> Option<()> {
        self.0.try_lock().then_some(())
    }
}

impl RawNestedLock for Mutex {
    fn lock_nested(&self, _subclass: u32) {
        self.lock()
    }
}
