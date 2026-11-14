// SPDX-License-Identifier: GPL-2.0

//! Lockdep: a lock's map and the hooks on it, for locks whose state is ours -
//! six, ByteLock - so no C lock tells lockdep about them; and the C locks'
//! asserts.

use crate::c;
use crate::locking::{mutex, percpu_rw_semaphore, raw_spinlock_t, rw_semaphore, spinlock_t};
use crate::util::ffi::c_char;
use core::ffi::{c_ulong, c_void};
use core::ptr;

/// The lockdep class key of the call site: a static of its own.
#[macro_export]
macro_rules! lock_class_key {
    () => { ::kernel::static_lock_class!().as_ptr().cast() };
}

/// A lock's struct lockdep_map. Without lockdep it's nothing, as C's is then
/// too - an empty struct - so a struct C shares declares one either way. (C's
/// is empty without CONFIG_LOCKDEP; everything that turns that on turns on
/// CONFIG_DEBUG_LOCK_ALLOC, which the hooks are under.)
#[repr(C)]
pub struct LockdepMap(
    #[cfg(CONFIG_DEBUG_LOCK_ALLOC)] crate::util::ffi::Opaque<crate::cstructs::c::lockdep_map>,
    // C's empty struct: improper_ctypes won't have a struct with no fields
    #[cfg(not(CONFIG_DEBUG_LOCK_ALLOC))] [u8; 0],
);

#[cfg(CONFIG_DEBUG_LOCK_ALLOC)]
impl LockdepMap {
    fn raw(&self) -> *mut c_void {
        self.0.as_ptr().cast()
    }

    /// # Safety
    /// Before its lock is first taken. @key lives forever.
    pub unsafe fn init(this: *mut Self, name: *const c_char, key: *mut c_void) {
        unsafe { c::rust_lockdep_init_map(this.cast(), name, key) }
    }

    pub fn acquire(&self, trylock: bool, read: bool, ip: c_ulong) {
        unsafe { c::rust_lock_acquire(self.raw(), trylock.into(), read.into(), ip) }
    }

    pub fn release(&self, ip: c_ulong) {
        unsafe { c::rust_lock_release(self.raw(), ip) }
    }

    pub fn contended(&self, ip: c_ulong) {
        unsafe { c::rust_lock_contended(self.raw(), ip) }
    }

    pub fn acquired(&self, ip: c_ulong) {
        unsafe { c::rust_lock_acquired(self.raw(), ip) }
    }

    /// For a lock lockdep doesn't track: btree node locks.
    pub fn set_notrack_class(&self) {
        unsafe { c::rust_lockdep_set_notrack_class(self.raw().cast()) }
    }
}

#[cfg(not(CONFIG_DEBUG_LOCK_ALLOC))]
impl LockdepMap {
    /// # Safety
    /// None: there's nothing to initialize.
    pub unsafe fn init(_this: *mut Self, _name: *const c_char, _key: *mut c_void) {}
    pub fn acquire(&self, _trylock: bool, _read: bool, _ip: c_ulong) {}
    pub fn release(&self, _ip: c_ulong) {}
    pub fn contended(&self, _ip: c_ulong) {}
    pub fn acquired(&self, _ip: c_ulong) {}
    pub fn set_notrack_class(&self) {}
}

/// That no lock lockdep knows of is in the @len bytes at @p, about to be
/// freed or reused.
pub fn debug_check_no_locks_freed(p: *const c_void, len: usize) {
    #[cfg(CONFIG_DEBUG_LOCK_ALLOC)]
    unsafe { c::rust_debug_check_no_locks_freed(p, len as c_ulong) };
    #[cfg(not(CONFIG_DEBUG_LOCK_ALLOC))]
    let _ = (p, len);
}

pub fn mutex_assert_held(l: &mutex) {
    unsafe { c::rust_mutex_assert_held(ptr::from_ref(l).cast_mut().cast()) }
}

pub fn spin_lock_assert_held(l: &spinlock_t) {
    unsafe { c::rust_spin_lock_assert_held(ptr::from_ref(l).cast_mut().cast()) }
}

pub fn raw_spin_lock_assert_held(l: &raw_spinlock_t) {
    unsafe { c::rust_raw_spin_lock_assert_held(ptr::from_ref(l).cast_mut().cast()) }
}

pub fn rwsem_assert_held(l: &rw_semaphore) {
    unsafe { c::rust_rwsem_assert_held(ptr::from_ref(l).cast_mut().cast()) }
}

pub fn rwsem_assert_held_write(l: &rw_semaphore) {
    unsafe { c::rust_rwsem_assert_held_write(ptr::from_ref(l).cast_mut().cast()) }
}

pub fn percpu_rwsem_assert_held(l: &percpu_rw_semaphore) {
    unsafe { c::rust_percpu_rwsem_assert_held(ptr::from_ref(l).cast_mut().cast()) }
}

pub fn percpu_rwsem_assert_held_write(l: &percpu_rw_semaphore) {
    unsafe { c::rust_percpu_rwsem_assert_held_write(ptr::from_ref(l).cast_mut().cast()) }
}
