// SPDX-License-Identifier: GPL-2.0

//! PercpuRwSem: the kernel's percpu_rw_semaphore, as a RawRwLock.
//! Initializing in a lockdep class is a macro: shim.c's
//! rust_percpu_init_rwsem(), and the rest of its rust_percpu_* wrappers.

use crate::locking::{RawLock, RawLockBase, RawReadLock, RawRwLock};
use crate::errcode::{ret_to_result_void, BchError};
use crate::util::ffi::Opaque;
use core::ffi::CStr;

use kernel::bindings as b;

pub type PercpuRwSem = Opaque<b::percpu_rw_semaphore>;

// taken and released from any thread
unsafe impl Send for PercpuRwSem {}
unsafe impl Sync for PercpuRwSem {}

pub use crate::c::{
    rust_percpu_init_rwsem, rust_percpu_free_rwsem, rust_percpu_down_read, rust_percpu_up_read,
    rust_percpu_down_write, rust_percpu_up_write,
};
use crate::os::lockdep::{self, LockClass};

impl RawLockBase for PercpuRwSem {
    type GuardState = ();
    /// The per-CPU counts couldn't be allocated
    type InitError = BchError;

    unsafe fn init(l: *mut Self, name: &'static CStr, key: LockClass) -> Result<(), BchError> {
        ret_to_result_void(unsafe { rust_percpu_init_rwsem(l.cast(), name.as_ptr().cast(), key.as_ptr().cast()) })
    }

    unsafe fn destroy(&self) {
        unsafe { rust_percpu_free_rwsem(self.as_ptr().cast()) }
    }
}

impl RawLock for PercpuRwSem {
    fn lock(&self) {
        unsafe { rust_percpu_down_write(self.as_ptr().cast()) }
    }

    unsafe fn unlock(&self, _: ()) {
        unsafe { rust_percpu_up_write(self.as_ptr().cast()) }
    }

    fn assert_held(&self) {
        lockdep::percpu_rwsem_assert_held_write(self)
    }
}

impl RawReadLock for PercpuRwSem {
    fn lock_read(&self) {
        unsafe { rust_percpu_down_read(self.as_ptr().cast()) }
    }

    unsafe fn unlock_read(&self, _: ()) {
        unsafe { rust_percpu_up_read(self.as_ptr().cast()) }
    }
}

impl RawRwLock for PercpuRwSem {
    fn assert_held_any(&self) {
        lockdep::percpu_rwsem_assert_held(self)
    }
}
