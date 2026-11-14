// SPDX-License-Identifier: GPL-2.0

//! Lockdep, for userspace: there's none, so every hook does nothing.

use crate::locking::percpu_rw_semaphore;
use crate::util::ffi::c_char;
use core::ffi::{c_ulong, c_void};

/// No lockdep, no class: a null key.
#[macro_export]
macro_rules! lock_class_key {
    () => { ::core::ptr::null_mut() };
}

/// C's struct lockdep_map without lockdep: an empty struct.
#[repr(C)]
pub struct LockdepMap([u8; 0]);

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

pub fn debug_check_no_locks_freed(_p: *const c_void, _len: usize) {}

pub fn trace_contention_begin<L>(_lock: &L, _flags: core::ffi::c_uint) {}
pub fn trace_contention_end<L>(_lock: &L, _ret: core::ffi::c_int) {}

pub fn percpu_rwsem_assert_held(_: &percpu_rw_semaphore) {}
pub fn percpu_rwsem_assert_held_write(_: &percpu_rw_semaphore) {}
