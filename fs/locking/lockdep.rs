// SPDX-License-Identifier: GPL-2.0

//! Lockdep, for locks whose state is ours - six, ByteLock - so no C lock
//! tells lockdep about them.
//!
//! LockdepMap is a lock's struct lockdep_map, and lockdep's hooks on it.
//! Without lockdep it's nothing, as C's is then too - an empty struct - so a
//! struct C shares declares one either way. (C's is empty without
//! CONFIG_LOCKDEP; everything that turns that on turns on
//! CONFIG_DEBUG_LOCK_ALLOC, which the hooks are under.)

use crate::util::ffi::c_char;
use core::ffi::{c_ulong, c_void};

#[cfg(CONFIG_DEBUG_LOCK_ALLOC)]
use super::raw::{rust_lock_acquire, rust_lock_acquired, rust_lock_contended, rust_lock_release,
                 rust_lockdep_init_map};

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
        unsafe { rust_lockdep_init_map(this.cast(), name, key) }
    }

    pub fn acquire(&self, trylock: bool, read: bool, ip: c_ulong) {
        unsafe { rust_lock_acquire(self.raw(), trylock.into(), read.into(), ip) }
    }

    pub fn release(&self, ip: c_ulong) {
        unsafe { rust_lock_release(self.raw(), ip) }
    }

    pub fn contended(&self, ip: c_ulong) {
        unsafe { rust_lock_contended(self.raw(), ip) }
    }

    pub fn acquired(&self, ip: c_ulong) {
        unsafe { rust_lock_acquired(self.raw(), ip) }
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
}
