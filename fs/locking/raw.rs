// SPDX-License-Identifier: GPL-2.0

//! C's locks, as RawLocks: struct mutex, rw_semaphore and
//! percpu_rw_semaphore - in both builds, the shim's pthread-backed ones in
//! userspace.
//!
//! What C has as a macro or a static inline in one build or the other -
//! initializing in a lockdep class, lockdep's asserts - Rust calls through
//! locking.c's rust_* functions.

use super::RawLock;
use super::RawRwLock;
use crate::c;
use crate::errcode::{ret_to_result_void, BchError};
use crate::util::ffi::c_char;
use core::convert::Infallible;
use core::ffi::{c_int, c_void, CStr};
use cstruct_macros::c_extern;

c_extern! {
    pub fn rust_mutex_init(lock: *mut c::mutex, name: *const c_char, key: *mut c_void);
    pub fn rust_mutex_assert_held(lock: *mut c::mutex);

    pub fn rust_init_rwsem(sem: *mut c::rw_semaphore, name: *const c_char, key: *mut c_void);
    pub fn rust_rwsem_assert_held(sem: *mut c::rw_semaphore);
    pub fn rust_rwsem_assert_held_write(sem: *mut c::rw_semaphore);

    pub fn rust_percpu_init_rwsem(sem: *mut c::percpu_rw_semaphore, name: *const c_char,
                                  key: *mut c_void) -> c_int;
    pub fn rust_percpu_free_rwsem(sem: *mut c::percpu_rw_semaphore);
    pub fn rust_percpu_down_read(sem: *mut c::percpu_rw_semaphore);
    pub fn rust_percpu_up_read(sem: *mut c::percpu_rw_semaphore);
    pub fn rust_percpu_down_write(sem: *mut c::percpu_rw_semaphore);
    pub fn rust_percpu_up_write(sem: *mut c::percpu_rw_semaphore);
    pub fn rust_percpu_rwsem_assert_held(sem: *mut c::percpu_rw_semaphore);
    pub fn rust_percpu_rwsem_assert_held_write(sem: *mut c::percpu_rw_semaphore);
}

// ── struct mutex ─────────────────────────────────────────────────────────

unsafe impl RawLock for c::mutex {
    type GuardState = ();
    type InitError = Infallible;

    unsafe fn init(l: *mut Self, name: &'static CStr, key: *mut c_void) -> Result<(), Infallible> {
        unsafe { rust_mutex_init(l, name.as_ptr().cast(), key) };
        Ok(())
    }

    unsafe fn lock(l: *mut Self) {
        unsafe { c::mutex_lock(l) }
    }

    unsafe fn unlock(l: *mut Self, _: ()) {
        unsafe { c::mutex_unlock(l) }
    }

    unsafe fn assert_held(l: *mut Self) {
        unsafe { rust_mutex_assert_held(l) }
    }
}

// ── rw_semaphore ─────────────────────────────────────────────────────────

unsafe impl RawLock for c::rw_semaphore {
    type GuardState = ();
    type InitError = Infallible;

    unsafe fn init(l: *mut Self, name: &'static CStr, key: *mut c_void) -> Result<(), Infallible> {
        unsafe { rust_init_rwsem(l, name.as_ptr().cast(), key) };
        Ok(())
    }

    unsafe fn lock(l: *mut Self) {
        unsafe { c::down_write(l) }
    }

    unsafe fn unlock(l: *mut Self, _: ()) {
        unsafe { c::up_write(l) }
    }

    unsafe fn assert_held(l: *mut Self) {
        unsafe { rust_rwsem_assert_held_write(l) }
    }
}

unsafe impl RawRwLock for c::rw_semaphore {
    unsafe fn lock_shared(l: *mut Self) {
        unsafe { c::down_read(l) }
    }

    unsafe fn unlock_shared(l: *mut Self, _: ()) {
        unsafe { c::up_read(l) }
    }

    unsafe fn assert_held_any(l: *mut Self) {
        unsafe { rust_rwsem_assert_held(l) }
    }
}

// ── percpu_rw_semaphore ──────────────────────────────────────────────────

unsafe impl RawLock for c::percpu_rw_semaphore {
    type GuardState = ();
    /// The per-CPU counts couldn't be allocated
    type InitError = BchError;

    unsafe fn init(l: *mut Self, name: &'static CStr, key: *mut c_void) -> Result<(), BchError> {
        ret_to_result_void(unsafe { rust_percpu_init_rwsem(l, name.as_ptr().cast(), key) })
    }

    unsafe fn lock(l: *mut Self) {
        unsafe { rust_percpu_down_write(l) }
    }

    unsafe fn unlock(l: *mut Self, _: ()) {
        unsafe { rust_percpu_up_write(l) }
    }

    unsafe fn assert_held(l: *mut Self) {
        unsafe { rust_percpu_rwsem_assert_held_write(l) }
    }

    unsafe fn destroy(l: *mut Self) {
        unsafe { rust_percpu_free_rwsem(l) }
    }
}

unsafe impl RawRwLock for c::percpu_rw_semaphore {
    unsafe fn lock_shared(l: *mut Self) {
        unsafe { rust_percpu_down_read(l) }
    }

    unsafe fn unlock_shared(l: *mut Self, _: ()) {
        unsafe { rust_percpu_up_read(l) }
    }

    unsafe fn assert_held_any(l: *mut Self) {
        unsafe { rust_percpu_rwsem_assert_held(l) }
    }
}
