// SPDX-License-Identifier: GPL-2.0

//! The kernel's struct mutex, as a RawLock. Initializing in a lockdep class
//! is a macro: shim.c's rust_mutex_init().

#![allow(non_camel_case_types)]

use crate::c;
use crate::locking::{RawLock, RawLockBase, RawNestedLock, RawTryLock};
use crate::os::lockdep;
use crate::util::ffi::Opaque;
use core::convert::Infallible;
use core::ffi::{c_void, CStr};
use kernel::bindings as b;

pub type mutex = Opaque<b::mutex>;

// Zeroed as the structs holding them start; a lock only once their init runs
unsafe impl ::pin_init::Zeroable for mutex {}

/// How two locks of one class, by their maps, are ordered: negative for @a
/// first. Lockdep's lock_cmp_fn, as kCFI hashes it.
#[cfg(CONFIG_PROVE_LOCKING)]
pub type LockCmpFn = unsafe extern "C" fn(a: *const b::lockdep_map,
                                          b: *const b::lockdep_map) -> core::ffi::c_int;

#[cfg(CONFIG_PROVE_LOCKING)]
impl<G, P> crate::locking::Locked<mutex, G, P> {
    /// Lockdep's order among the mutexes of its class held at once, @cmp's:
    /// lock_set_cmp_fn().
    pub fn set_cmp_fn(&self, cmp: LockCmpFn) {
        let l = core::ptr::from_ref(self.raw()).cast_mut().cast::<b::mutex>();
        unsafe { b::lockdep_set_lock_cmp_fn(&raw mut (*l).dep_map, Some(cmp), None) }
    }
}

unsafe impl RawLockBase for mutex {
    type GuardState = ();
    type InitError = Infallible;

    unsafe fn init(l: *mut Self, name: &'static CStr, key: *mut c_void) -> Result<(), Infallible> {
        unsafe { c::rust_mutex_init(l.cast(), name.as_ptr().cast(), key) };
        Ok(())
    }
}

unsafe impl RawLock for mutex {
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

unsafe impl RawTryLock for mutex {
    unsafe fn try_lock(&self) -> Option<()> {
        (unsafe { c::mutex_trylock(self.as_ptr().cast()) } as i32 != 0).then_some(())
    }
}

// Without lockdep's lock tracking, mutex_lock_nested() is a macro, mutex_lock()
unsafe impl RawNestedLock for mutex {
    #[cfg(CONFIG_DEBUG_LOCK_ALLOC)]
    unsafe fn lock_nested(&self, subclass: u32) {
        unsafe { c::mutex_lock_nested(self.as_ptr().cast(), subclass) }
    }

    #[cfg(not(CONFIG_DEBUG_LOCK_ALLOC))]
    unsafe fn lock_nested(&self, _subclass: u32) {
        unsafe { self.lock() }
    }
}
