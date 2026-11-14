// SPDX-License-Identifier: GPL-2.0

//! C's percpu_rw_semaphore, as a RawRwLock - in both builds, the shim's
//! pthread-backed one in userspace. The spinlocks, rw_semaphore and struct
//! mutex are the ports' (os::spinlock, os::rwsem, os::mutex).
//!
//! It's C's struct in an Opaque - an UnsafeCell, so it's written through a
//! shared reference: by whoever takes it, Rust or C. c::percpu_rw_semaphore
//! is this, over the bindings' (cstructs.rs), so the structs C shares hold
//! it as written - and Locked::from_c() borrows one soundly.
//!
//! What C has as a macro or a static inline in one build or the other -
//! initializing in a lockdep class - Rust calls through locking.c's rust_*
//! functions.

#![allow(non_camel_case_types)]

use super::{RawLock, RawLockBase};
use super::{RawReadLock, RawRwLock};
use crate::c;
use crate::errcode::{ret_to_result_void, BchError};
use crate::util::ffi::Opaque;
use core::ffi::{c_void, CStr};

#[cfg(kernel)]
use kernel::bindings as b;
#[cfg(not(kernel))]
use bcachefs_shim::c as b;

pub type percpu_rw_semaphore = Opaque<b::percpu_rw_semaphore>;

// Zeroed as the structs holding them start; a lock only once their init runs
unsafe impl ::pin_init::Zeroable for percpu_rw_semaphore {}

pub use crate::c::{
    rust_trace_contention_begin, rust_trace_contention_end,
    rust_local_lock_init, rust_local_lock_irqsave, rust_local_unlock_irqrestore,
    rust_local_irq_save, rust_local_irq_restore,
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
