// SPDX-License-Identifier: GPL-2.0

//! RwSem: the kernel's rw_semaphore, as a RawRwLock. Initializing in a
//! lockdep class is a macro: shim.c's rust_init_rwsem().

use crate::c;
use crate::locking::{RawLock, RawLockBase, RawReadLock, RawRwLock, RawTryReadLock};
use crate::os::lockdep::{self, LockClass};
use crate::util::ffi::Opaque;
use core::convert::Infallible;
use core::ffi::CStr;
use kernel::bindings as b;

pub type RwSem = Opaque<b::rw_semaphore>;

unsafe impl RawLockBase for RwSem {
    type GuardState = ();
    type InitError = Infallible;

    unsafe fn init(l: *mut Self, name: &'static CStr, key: LockClass) -> Result<(), Infallible> {
        unsafe { c::rust_init_rwsem(l.cast(), name.as_ptr().cast(), key.as_ptr().cast()) };
        Ok(())
    }
}

unsafe impl RawLock for RwSem {
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

unsafe impl RawReadLock for RwSem {
    unsafe fn lock_read(&self) {
        unsafe { c::down_read(self.as_ptr().cast()) }
    }

    unsafe fn unlock_read(&self, _: ()) {
        unsafe { c::up_read(self.as_ptr().cast()) }
    }
}

unsafe impl RawTryReadLock for RwSem {
    unsafe fn try_lock_read(&self) -> Option<()> {
        (unsafe { c::down_read_trylock(self.as_ptr().cast()) } != 0).then_some(())
    }
}

unsafe impl RawRwLock for RwSem {
    unsafe fn assert_held_any(&self) {
        lockdep::rwsem_assert_held(self)
    }
}
