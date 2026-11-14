// SPDX-License-Identifier: GPL-2.0

//! C's locks, as RawLocks: struct mutex, rw_semaphore and
//! percpu_rw_semaphore - in both builds, the shim's pthread-backed ones in
//! userspace. The spinlocks are the ports' (os::spinlock).
//!
//! Each is C's struct in an Opaque - an UnsafeCell, so it's written through
//! a shared reference: by whoever takes it, Rust or C. c::mutex and the rest
//! are these, over the bindings' (cstructs.rs), so the structs C shares hold
//! them as written - and Locked::from_c() borrows one soundly.
//!
//! What C has as a macro or a static inline in one build or the other -
//! initializing in a lockdep class - Rust calls through locking.c's rust_*
//! functions.

#![allow(non_camel_case_types)]

use super::{RawLock, RawLockBase};
use super::{RawReadLock, RawRwLock, RawTryReadLock};
use super::{RawNestedLock, RawTryLock};
use crate::c;
use crate::errcode::{ret_to_result_void, BchError};
use crate::util::ffi::Opaque;
use core::convert::Infallible;
use core::ffi::{c_void, CStr};

#[cfg(kernel)]
use kernel::bindings as b;
#[cfg(not(kernel))]
use bcachefs_shim::c as b;

pub type mutex = Opaque<b::mutex>;
pub type rw_semaphore = Opaque<b::rw_semaphore>;
pub type percpu_rw_semaphore = Opaque<b::percpu_rw_semaphore>;

// Zeroed as the structs holding them start; a lock only once their init runs
unsafe impl ::pin_init::Zeroable for mutex {}
unsafe impl ::pin_init::Zeroable for rw_semaphore {}
unsafe impl ::pin_init::Zeroable for percpu_rw_semaphore {}

pub use crate::c::{
    rust_mutex_init,
    rust_trace_contention_begin, rust_trace_contention_end,
    rust_local_lock_init, rust_local_lock_irqsave, rust_local_unlock_irqrestore,
    rust_local_irq_save, rust_local_irq_restore,
    rust_init_rwsem,
    rust_percpu_init_rwsem, rust_percpu_free_rwsem, rust_percpu_down_read, rust_percpu_up_read,
    rust_percpu_down_write, rust_percpu_up_write,
};
use crate::os::lockdep;

// ── struct mutex ─────────────────────────────────────────────────────────

/// How two locks of one class, by their maps, are ordered: negative for @a
/// first. Lockdep's lock_cmp_fn, as kCFI hashes it.
#[cfg(all(kernel, CONFIG_PROVE_LOCKING))]
pub type LockCmpFn = unsafe extern "C" fn(a: *const kernel::bindings::lockdep_map,
                                          b: *const kernel::bindings::lockdep_map) -> core::ffi::c_int;

#[cfg(all(kernel, CONFIG_PROVE_LOCKING))]
impl<G, P> super::Locked<c::mutex, G, P> {
    /// Lockdep's order among the mutexes of its class held at once, @cmp's:
    /// lock_set_cmp_fn().
    pub fn set_cmp_fn(&self, cmp: LockCmpFn) {
        let l = core::ptr::from_ref(self.raw()).cast_mut().cast::<kernel::bindings::mutex>();
        unsafe { kernel::bindings::lockdep_set_lock_cmp_fn(&raw mut (*l).dep_map, Some(cmp), None) }
    }
}

unsafe impl RawLockBase for c::mutex {
    type GuardState = ();
    type InitError = Infallible;

    unsafe fn init(l: *mut Self, name: &'static CStr, key: *mut c_void) -> Result<(), Infallible> {
        unsafe { rust_mutex_init(l.cast(), name.as_ptr().cast(), key) };
        Ok(())
    }
}

unsafe impl RawLock for c::mutex {
    unsafe fn lock(&self) {
        unsafe { c::mutex_lock(self.as_ptr().cast()) }
    }

    unsafe fn unlock(&self, _: ()) {
        unsafe { c::mutex_unlock(self.as_ptr().cast()) }
    }

    unsafe fn assert_held(&self) {
        lockdep::mutex_assert_held(self)
    }
}

unsafe impl RawTryLock for c::mutex {
    unsafe fn try_lock(&self) -> Option<()> {
        (unsafe { c::mutex_trylock(self.as_ptr().cast()) } as i32 != 0).then_some(())
    }
}

// Without lockdep's lock tracking, mutex_lock_nested() is a macro, mutex_lock()
unsafe impl RawNestedLock for c::mutex {
    #[cfg(CONFIG_DEBUG_LOCK_ALLOC)]
    unsafe fn lock_nested(&self, subclass: u32) {
        unsafe { c::mutex_lock_nested(self.as_ptr().cast(), subclass) }
    }

    #[cfg(not(CONFIG_DEBUG_LOCK_ALLOC))]
    unsafe fn lock_nested(&self, _subclass: u32) {
        unsafe { self.lock() }
    }
}

// ── rw_semaphore ─────────────────────────────────────────────────────────

unsafe impl RawLockBase for c::rw_semaphore {
    type GuardState = ();
    type InitError = Infallible;

    unsafe fn init(l: *mut Self, name: &'static CStr, key: *mut c_void) -> Result<(), Infallible> {
        unsafe { rust_init_rwsem(l.cast(), name.as_ptr().cast(), key) };
        Ok(())
    }
}

unsafe impl RawLock for c::rw_semaphore {
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

unsafe impl RawReadLock for c::rw_semaphore {
    unsafe fn lock_read(&self) {
        unsafe { c::down_read(self.as_ptr().cast()) }
    }

    unsafe fn unlock_read(&self, _: ()) {
        unsafe { c::up_read(self.as_ptr().cast()) }
    }
}

unsafe impl RawTryReadLock for c::rw_semaphore {
    unsafe fn try_lock_read(&self) -> Option<()> {
        (unsafe { c::down_read_trylock(self.as_ptr().cast()) } != 0).then_some(())
    }
}

unsafe impl RawRwLock for c::rw_semaphore {
    unsafe fn assert_held_any(&self) {
        lockdep::rwsem_assert_held(self)
    }
}

// ── percpu_rw_semaphore ──────────────────────────────────────────────────

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
