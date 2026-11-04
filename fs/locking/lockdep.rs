// SPDX-License-Identifier: GPL-2.0

//! Lockdep, for locks whose state is ours - six, ByteLock - so no C lock
//! tells lockdep about them.
//!
//! LockdepMap is a lock's struct lockdep_map, and lockdep's hooks on it.
//! Without lockdep it's nothing, as C's is then too - an empty struct - so a
//! struct C shares declares one either way. (C's is empty without
//! CONFIG_LOCKDEP; everything that turns that on turns on
//! CONFIG_DEBUG_LOCK_ALLOC, which the hooks are under.)
//!
//! Lockdep<R> is any of those locks R, which lockdep knows about: R, then its
//! map - Lockdep<ByteLock> is C's struct byte_lock_lockdep. Taken as C's
//! LOCK_CONTENDED() takes a mutex. Not for what there are millions of: a
//! ByteLock in an array stays one byte.

use super::{RawLock, RawLockBase, RawTimedLock, RawTryLock};
use crate::util::ffi::c_char;
use core::ffi::{c_ulong, c_void, CStr};
use cstruct_macros::c_verbatim;

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

/// Lock @R, which lockdep knows about. No caller ip: C's is the call site's,
/// _THIS_IP_, which Rust has no equivalent of.
#[repr(C)]
pub struct Lockdep<R> {
    raw:     R,
    dep_map: LockdepMap,
}

c_verbatim!(r#"
/* Lockdep<ByteLock>: */
struct byte_lock_lockdep {
	u8			lock;
	struct lockdep_map	dep_map;
};

static inline void byte_lock_lockdep_unlock(struct byte_lock_lockdep *l)
{
	lock_release(&l->dep_map, _THIS_IP_);
	bch2_byte_unlock(&l->lock);
}
"#);

unsafe impl<R: RawLockBase> RawLockBase for Lockdep<R> {
    type GuardState = R::GuardState;
    type InitError = R::InitError;

    unsafe fn init(l: *mut Self, name: &'static CStr, key: *mut c_void) -> Result<(), R::InitError> {
        unsafe {
            R::init(&raw mut (*l).raw, name, key)?;
            LockdepMap::init(&raw mut (*l).dep_map, name.as_ptr().cast(), key);
        }
        Ok(())
    }

    unsafe fn destroy(&self) {
        unsafe { self.raw.destroy() }
    }
}

unsafe impl<R: RawTryLock> RawLock for Lockdep<R> {
    unsafe fn lock(&self) -> R::GuardState {
        self.dep_map.acquire(false, false, 0);
        let g = unsafe { self.raw.try_lock() }.unwrap_or_else(|| {
            self.dep_map.contended(0);
            unsafe { self.raw.lock() }
        });
        self.dep_map.acquired(0);
        g
    }

    unsafe fn unlock(&self, g: R::GuardState) {
        self.dep_map.release(0);
        unsafe { self.raw.unlock(g) }
    }

    unsafe fn assert_held(&self) {
        unsafe { self.raw.assert_held() }
    }
}

unsafe impl<R: RawTryLock> RawTryLock for Lockdep<R> {
    unsafe fn try_lock(&self) -> Option<R::GuardState> {
        let g = unsafe { self.raw.try_lock() }?;
        self.dep_map.acquire(true, false, 0);
        Some(g)
    }
}

unsafe impl<R: RawTimedLock> RawTimedLock for Lockdep<R> {
    unsafe fn try_lock_timeout(&self, timeout: c_ulong) -> Option<R::GuardState> {
        self.dep_map.acquire(false, false, 0);
        let g = unsafe { self.raw.try_lock() }.or_else(|| {
            self.dep_map.contended(0);
            unsafe { self.raw.try_lock_timeout(timeout) }
        });
        match g {
            Some(_) => self.dep_map.acquired(0),
            None    => self.dep_map.release(0),
        }
        g
    }
}
