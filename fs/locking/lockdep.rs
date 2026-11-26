// SPDX-License-Identifier: GPL-2.0

//! Lockdep, for locks whose state is ours - six, ByteLock - so no C lock
//! tells lockdep about them: os::lockdep's LockdepMap and its hooks.
//!
//! Lockdep<R> is any of those locks R, which lockdep knows about: R, then its
//! map - Lockdep<ByteLock> is C's struct byte_lock_lockdep. Taken as C's
//! LOCK_CONTENDED() takes a mutex. Not for what there are millions of: a
//! ByteLock in an array stays one byte.

use super::{RawLock, RawLockBase, RawTimedLock, RawTryLock};
use crate::os::lockdep::{LockClass, LockdepMap};
use core::ffi::CStr;

/// A LockdepMap's initializer, its class the call site's, named @name:
/// lockdep_init_map().
#[macro_export]
macro_rules! new_lockdep_map {
    ($name:literal $(,)?) => {{
        let key = $crate::new_lock!(@key);
        let name = const { $crate::util::ffi::c_str(::core::concat!($name, "\0")) };
        // SAFETY: before anything takes it; a static key per call site
        unsafe {
            ::pin_init::pin_init_from_closure(move |m: *mut $crate::locking::LockdepMap| {
                $crate::locking::LockdepMap::init(m, name.as_ptr().cast(), key);
                Ok::<(), ::core::convert::Infallible>(())
            })
        }
    }};
}

/// Lock @R, which lockdep knows about. No caller ip: C's is the call site's,
/// _THIS_IP_, which Rust has no equivalent of.
#[repr(C)]
pub struct Lockdep<R> {
    raw:     R,
    dep_map: LockdepMap,
}

impl<R: RawLockBase> RawLockBase for Lockdep<R> {
    type GuardState = R::GuardState;
    type InitError = R::InitError;

    unsafe fn init(l: *mut Self, name: &'static CStr, key: LockClass) -> Result<(), R::InitError> {
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

impl<R: RawTryLock> RawLock for Lockdep<R> {
    fn lock(&self) -> R::GuardState {
        self.dep_map.acquire(false, false, 0);
        let g = self.raw.try_lock().unwrap_or_else(|| {
            self.dep_map.contended(0);
            self.raw.lock()
        });
        self.dep_map.acquired(0);
        g
    }

    unsafe fn unlock(&self, g: R::GuardState) {
        self.dep_map.release(0);
        unsafe { self.raw.unlock(g) }
    }

    fn assert_held(&self) {
        self.raw.assert_held()
    }
}

impl<R: RawTryLock> RawTryLock for Lockdep<R> {
    fn try_lock(&self) -> Option<R::GuardState> {
        let g = self.raw.try_lock()?;
        self.dep_map.acquire(true, false, 0);
        Some(g)
    }
}

impl<R: RawTimedLock> RawTimedLock for Lockdep<R> {
    fn try_lock_timeout(&self, timeout: usize) -> Option<R::GuardState> {
        self.dep_map.acquire(false, false, 0);
        let g = self.raw.try_lock().or_else(|| {
            self.dep_map.contended(0);
            self.raw.try_lock_timeout(timeout)
        });
        match g {
            Some(_) => self.dep_map.acquired(0),
            None    => self.dep_map.release(0),
        }
        g
    }
}
