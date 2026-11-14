// SPDX-License-Identifier: GPL-2.0

//! The kernel's percpu_rw_semaphore, as a RawRwLock. Initializing in a lockdep
//! class is a macro: shim.c's rust_percpu_init_rwsem(), and the rest of its
//! rust_percpu_* wrappers.

#![allow(non_camel_case_types)]

use crate::locking::{RawLock, RawLockBase, RawReadLock, RawRwLock};
use crate::c;
use crate::errcode::{ret_to_result_void, BchError};
use crate::util::ffi::Opaque;
use core::ffi::{c_void, CStr};

use kernel::bindings as b;

pub type percpu_rw_semaphore = Opaque<b::percpu_rw_semaphore>;

// Zeroed as the structs holding them start; a lock only once their init runs
unsafe impl ::pin_init::Zeroable for percpu_rw_semaphore {}

pub use crate::c::{
    rust_percpu_init_rwsem, rust_percpu_free_rwsem, rust_percpu_down_read, rust_percpu_up_read,
    rust_percpu_down_write, rust_percpu_up_write,
};
use crate::os::lockdep;

unsafe impl RawLockBase for c::percpu_rw_semaphore {
    type GuardState = ();
    /// The per-CPU counts couldn't be allocated
    type InitError = BchError;

    unsafe fn init(l: *mut Self, name: &'static CStr, key: *mut c_void) -> Result<(), BchError> {
        ret_to_result_void(unsafe { rust_percpu_init_rwsem(l.cast(), name.as_ptr().cast(), key) })
    }

    unsafe fn destroy(&self) {
        unsafe { rust_percpu_free_rwsem(self.as_ptr().cast()) }
    }
}

unsafe impl RawLock for c::percpu_rw_semaphore {
    unsafe fn lock(&self) {
        unsafe { rust_percpu_down_write(self.as_ptr().cast()) }
    }

    unsafe fn unlock(&self, _: ()) {
        unsafe { rust_percpu_up_write(self.as_ptr().cast()) }
    }

    unsafe fn assert_held(&self) {
        lockdep::percpu_rwsem_assert_held_write(self)
    }
}

unsafe impl RawReadLock for c::percpu_rw_semaphore {
    unsafe fn lock_read(&self) {
        unsafe { rust_percpu_down_read(self.as_ptr().cast()) }
    }

    unsafe fn unlock_read(&self, _: ()) {
        unsafe { rust_percpu_up_read(self.as_ptr().cast()) }
    }
}

unsafe impl RawRwLock for c::percpu_rw_semaphore {
    unsafe fn assert_held_any(&self) {
        lockdep::percpu_rwsem_assert_held(self)
    }
}
