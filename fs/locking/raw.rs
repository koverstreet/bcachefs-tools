// SPDX-License-Identifier: GPL-2.0

//! C's locks, as RawLocks: struct mutex, spinlock_t (taken irqsave),
//! rw_semaphore and percpu_rw_semaphore - in both builds, the shim's
//! pthread-backed ones in userspace.
//!
//! Each is C's struct in an Opaque - an UnsafeCell, so it's written through
//! a shared reference: by whoever takes it, Rust or C. c::mutex and the rest
//! are these, over the bindings' (cstructs.rs), so the structs C shares hold
//! them as written - and Lock::from_c() borrows one soundly.
//!
//! What C has as a macro or a static inline in one build or the other -
//! initializing in a lockdep class, lockdep's asserts, spin_lock_irqsave() -
//! Rust calls through locking.c's rust_* functions.

#![allow(non_camel_case_types)]

use super::RawLock;
use super::RawRwLock;
use super::RawTryLock;
use crate::c;
use crate::errcode::{ret_to_result_void, BchError};
use crate::util::ffi::{c_char, Opaque};
use core::convert::Infallible;
use core::ffi::{c_int, c_ulong, c_void, CStr};
use cstruct_macros::c_extern;

#[cfg(kernel)]
use kernel::bindings as b;
#[cfg(not(kernel))]
use bcachefs_shim::c as b;

pub type mutex = Opaque<b::mutex>;
pub type spinlock_t = Opaque<b::spinlock_t>;
pub type rw_semaphore = Opaque<b::rw_semaphore>;
pub type percpu_rw_semaphore = Opaque<b::percpu_rw_semaphore>;

// Zeroed as the structs holding them start; a lock only once their init runs
unsafe impl ::pin_init::Zeroable for mutex {}
unsafe impl ::pin_init::Zeroable for spinlock_t {}
unsafe impl ::pin_init::Zeroable for rw_semaphore {}
unsafe impl ::pin_init::Zeroable for percpu_rw_semaphore {}

c_extern! {
    pub fn rust_mutex_init(lock: *mut c::mutex, name: *const c_char, key: *mut c_void);
    pub fn rust_mutex_assert_held(lock: *mut c::mutex);

    pub fn rust_spin_lock_init(lock: *mut c::spinlock_t, name: *const c_char, key: *mut c_void);
    pub fn rust_spin_lock_irqsave(lock: *mut c::spinlock_t) -> c_ulong;
    pub fn rust_spin_unlock_irqrestore(lock: *mut c::spinlock_t, flags: c_ulong);
    pub fn rust_spin_lock_assert_held(lock: *mut c::spinlock_t);

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
        unsafe { c::mutex_lock(l.cast()) }
    }

    unsafe fn unlock(l: *mut Self, _: ()) {
        unsafe { c::mutex_unlock(l.cast()) }
    }

    unsafe fn assert_held(l: *mut Self) {
        unsafe { rust_mutex_assert_held(l) }
    }
}

unsafe impl RawTryLock for c::mutex {
    unsafe fn try_lock(l: *mut Self) -> Option<()> {
        (unsafe { c::mutex_trylock(l.cast()) } as i32 != 0).then_some(())
    }
}

// ── spinlock_t, taken irqsave ────────────────────────────────────────────

// Taken with interrupts off - spin_lock_irqsave() - for what interrupt
// context takes too, an endio; held, it mustn't sleep. The kernel crate's
// SpinLock leaves interrupts on. In userspace, where there are no
// interrupts, it's the shim's spinlock, a pthread mutex.
unsafe impl RawLock for c::spinlock_t {
    type GuardState = c_ulong;
    type InitError = Infallible;

    unsafe fn init(l: *mut Self, name: &'static CStr, key: *mut c_void) -> Result<(), Infallible> {
        unsafe { rust_spin_lock_init(l, name.as_ptr().cast(), key) };
        Ok(())
    }

    unsafe fn lock(l: *mut Self) -> c_ulong {
        unsafe { rust_spin_lock_irqsave(l) }
    }

    unsafe fn unlock(l: *mut Self, flags: c_ulong) {
        unsafe { rust_spin_unlock_irqrestore(l, flags) }
    }

    unsafe fn assert_held(l: *mut Self) {
        unsafe { rust_spin_lock_assert_held(l) }
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
        unsafe { c::down_write(l.cast()) }
    }

    unsafe fn unlock(l: *mut Self, _: ()) {
        unsafe { c::up_write(l.cast()) }
    }

    unsafe fn assert_held(l: *mut Self) {
        unsafe { rust_rwsem_assert_held_write(l) }
    }
}

unsafe impl RawRwLock for c::rw_semaphore {
    unsafe fn lock_shared(l: *mut Self) {
        unsafe { c::down_read(l.cast()) }
    }

    unsafe fn unlock_shared(l: *mut Self, _: ()) {
        unsafe { c::up_read(l.cast()) }
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
