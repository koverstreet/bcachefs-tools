// SPDX-License-Identifier: GPL-2.0

//! percpu_rw_semaphore, for userspace, as a RawRwLock: parking_lot's, as
//! rw_semaphore is - readers share it, as the kernel's do; there are no CPUs'
//! counts to allocate, so init can't fail.

#![allow(non_camel_case_types)]

use crate::errcode::BchError;
use crate::locking::{RawLock, RawLockBase, RawReadLock, RawRwLock};
use core::ffi::{c_void, CStr};
use parking_lot::lock_api::RawRwLock as _;

#[repr(transparent)]
pub struct percpu_rw_semaphore(parking_lot::RawRwLock);

unsafe impl ::pin_init::Zeroable for percpu_rw_semaphore {}

unsafe impl RawLockBase for percpu_rw_semaphore {
    type GuardState = ();
    type InitError = BchError;

    unsafe fn init(l: *mut Self, _name: &'static CStr, _key: *mut c_void) -> Result<(), BchError> {
        unsafe { l.write(percpu_rw_semaphore(parking_lot::RawRwLock::INIT)) };
        Ok(())
    }
}

unsafe impl RawLock for percpu_rw_semaphore {
    unsafe fn lock(&self) {
        self.0.lock_exclusive()
    }

    unsafe fn unlock(&self, _: ()) {
        unsafe { self.0.unlock_exclusive() }
    }

    unsafe fn assert_held(&self) {}
}

unsafe impl RawReadLock for percpu_rw_semaphore {
    unsafe fn lock_read(&self) {
        self.0.lock_shared()
    }

    unsafe fn unlock_read(&self, _: ()) {
        unsafe { self.0.unlock_shared() }
    }
}

unsafe impl RawRwLock for percpu_rw_semaphore {
    unsafe fn assert_held_any(&self) {}
}
