// SPDX-License-Identifier: GPL-2.0

//! A seqmutex: a mutex, and a count of the times it's been taken - so a
//! holder can drop it and take it back only if no one else took it
//! meanwhile, relock(). C's struct, with C's API in seqmutex.h. C reads the
//! count plainly, outside the lock; Rust, as an atomic.

#![allow(non_camel_case_types)]

use core::convert::Infallible;
use core::ffi::CStr;
use core::sync::atomic::{AtomicU32, Ordering};
use crate::locking::{ExclusiveGuard, Locked, Mutex, RawLock, RawLockBase, RawTryLock};
use crate::os::lockdep::LockClass;
use cstruct_macros::MaybeZeroable;

#[repr(C)]
#[derive(MaybeZeroable)]
pub struct Seqmutex {
    pub lock: Mutex,
    /* C writes it under the mutex */
    pub seq:  AtomicU32,
}

impl RawLockBase for Seqmutex {
    type GuardState = ();
    type InitError = Infallible;

    unsafe fn init(l: *mut Self, name: &'static CStr, key: LockClass) -> Result<(), Infallible> {
        unsafe {
            (&raw mut (*l).seq).write(AtomicU32::new(0));
            Mutex::init(&raw mut (*l).lock, name, key)
        }
    }
}

impl RawLock for Seqmutex {
    fn lock(&self) {
        self.lock.lock();
        self.seq.fetch_add(1, Ordering::Relaxed);
    }

    unsafe fn unlock(&self, _: ()) {
        unsafe { self.lock.unlock(()) }
    }

    fn assert_held(&self) {
        self.lock.assert_held()
    }
}

/// What relock() takes: the count at an unlock.
pub struct SeqMutexSeq(u32);

impl<G, P> Locked<Seqmutex, G, P> {
    /// Held again, if no one has taken it since the unlock that gave @at.
    pub fn relock(&self, at: SeqMutexSeq) -> Option<ExclusiveGuard<'_, Seqmutex, G, P>> {
        let l = self.raw();
        if l.seq.load(Ordering::Relaxed) != at.0 {
            return None;
        }

        // retaken, not counted: no one else has taken it
        l.lock.try_lock()?;
        let g = unsafe { ExclusiveGuard::from_held(self, ()) };

        if l.seq.load(Ordering::Relaxed) != at.0 {
            return None;    // unlocked, as the guard drops
        }
        Some(g)
    }
}

impl<G, P> ExclusiveGuard<'_, Seqmutex, G, P> {
    /// Released, with the count relock() takes.
    pub fn unlock(self) -> SeqMutexSeq {
        SeqMutexSeq(self.lock().raw().seq.load(Ordering::Relaxed))
    }
}
