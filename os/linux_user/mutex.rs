// SPDX-License-Identifier: GPL-2.0

//! struct mutex, for userspace, as a RawLock: parking_lot's - one byte, so an
//! unlocked one may be moved, where a copy of a pthread mutex isn't one.

#![allow(non_camel_case_types)]

use crate::locking::{RawLock, RawLockBase, RawNestedLock, RawTryLock};
use core::convert::Infallible;
use core::ffi::{c_void, CStr};
use parking_lot::lock_api::RawMutex as _;

#[repr(transparent)]
pub struct mutex(parking_lot::RawMutex);

unsafe impl ::pin_init::Zeroable for mutex {}

unsafe impl RawLockBase for mutex {
    type GuardState = ();
    type InitError = Infallible;

    unsafe fn init(l: *mut Self, _name: &'static CStr, _key: *mut c_void) -> Result<(), Infallible> {
        unsafe { l.write(mutex(parking_lot::RawMutex::INIT)) };
        Ok(())
    }
}

unsafe impl RawLock for mutex {
    unsafe fn lock(&self) {
        self.0.lock()
    }

    unsafe fn unlock(&self, _: ()) {
        unsafe { self.0.unlock() }
    }

    unsafe fn assert_held(&self) {}
}

unsafe impl RawTryLock for mutex {
    unsafe fn try_lock(&self) -> Option<()> {
        self.0.try_lock().then_some(())
    }
}

unsafe impl RawNestedLock for mutex {
    unsafe fn lock_nested(&self, _subclass: u32) {
        unsafe { self.lock() }
    }
}
