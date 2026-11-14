// SPDX-License-Identifier: GPL-2.0

//! The kernel's rw_semaphore, as a RawRwLock. Initializing in a lockdep
//! class is a macro: shim.c's rust_init_rwsem().

#![allow(non_camel_case_types)]

use crate::c;
use crate::locking::{RawLock, RawLockBase, RawReadLock, RawRwLock, RawTryReadLock};
use crate::os::lockdep;
use crate::util::ffi::Opaque;
use core::convert::Infallible;
use core::ffi::{c_void, CStr};
use kernel::bindings as b;

pub type rw_semaphore = Opaque<b::rw_semaphore>;

// Zeroed as the structs holding them start; a lock only once their init runs
unsafe impl ::pin_init::Zeroable for rw_semaphore {}

unsafe impl RawLockBase for rw_semaphore {
    type GuardState = ();
    type InitError = Infallible;

    unsafe fn init(l: *mut Self, name: &'static CStr, key: *mut c_void) -> Result<(), Infallible> {
        unsafe { c::rust_init_rwsem(l.cast(), name.as_ptr().cast(), key) };
        Ok(())
    }
}

unsafe impl RawLock for rw_semaphore {
    unsafe fn lock(&self) {
        unsafe { c::down_write(self.as_ptr().cast()) }
    }

    unsafe fn unlock(&self, _: ()) {
        unsafe { c::up_write(self.as_ptr().cast()) }
    }

    unsafe fn assert_held(&self) {
        lockdep::rwsem_assert_held_write(self)
    }
}

unsafe impl RawReadLock for rw_semaphore {
    unsafe fn lock_read(&self) {
        unsafe { c::down_read(self.as_ptr().cast()) }
    }

    unsafe fn unlock_read(&self, _: ()) {
        unsafe { c::up_read(self.as_ptr().cast()) }
    }
}

unsafe impl RawTryReadLock for rw_semaphore {
    unsafe fn try_lock_read(&self) -> Option<()> {
        (unsafe { c::down_read_trylock(self.as_ptr().cast()) } != 0).then_some(())
    }
}

unsafe impl RawRwLock for rw_semaphore {
    unsafe fn assert_held_any(&self) {
        lockdep::rwsem_assert_held(self)
    }
}
