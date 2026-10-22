// SPDX-License-Identifier: GPL-2.0

//! A seqmutex: a mutex, and a count of the times it's been taken - so a
//! holder can drop it and take it back only if no one else took it
//! meanwhile, relock(). C's struct, with C's API in seqmutex.h. C reads the
//! count plainly, outside the lock; Rust, as an atomic.

#![allow(non_camel_case_types)]

use super::{Guard, Lock, RawLock, RawTryLock};
use crate::cstructs::c;
use crate::types::c_default;
use core::convert::Infallible;
use core::ffi::{c_void, CStr};
use core::sync::atomic::{AtomicU32, Ordering};
use cstruct_macros::CStruct;

#[repr(C)]
#[derive(CStruct)]
pub struct seqmutex {
    pub lock: c::mutex,
    /* C writes it under the mutex */
    #[c("u32 seq")]
    pub seq:  AtomicU32,
}
c_default!(seqmutex);

unsafe impl RawLock for seqmutex {
    type GuardState = ();
    type InitError = Infallible;

    unsafe fn init(l: *mut Self, name: &'static CStr, key: *mut c_void) -> Result<(), Infallible> {
        unsafe {
            (&raw mut (*l).seq).write(AtomicU32::new(0));
            c::mutex::init(&raw mut (*l).lock, name, key)
        }
    }

    unsafe fn lock(&self) {
        unsafe { self.lock.lock() };
        self.seq.fetch_add(1, Ordering::Relaxed);
    }

    unsafe fn unlock(&self, _: ()) {
        unsafe { self.lock.unlock(()) }
    }

    unsafe fn assert_held(&self) {
        unsafe { self.lock.assert_held() }
    }
}

/// What relock() takes: the count at an unlock.
pub struct SeqMutexSeq(u32);

impl<G, P> Lock<seqmutex, G, P> {
    /// Held again, if no one has taken it since the unlock that gave @at.
    pub fn relock(&self, at: SeqMutexSeq) -> Option<Guard<'_, seqmutex, G, P>> {
        let l = self.raw();
        if l.seq.load(Ordering::Relaxed) != at.0 {
            return None;
        }

        // retaken, not counted: no one else has taken it
        unsafe { l.lock.try_lock() }?;
        let g = unsafe { Guard::from_held(self, Some(())) };

        if l.seq.load(Ordering::Relaxed) != at.0 {
            return None;    // unlocked, as the guard drops
        }
        Some(g)
    }
}

impl<G, P> Guard<'_, seqmutex, G, P> {
    /// Released, with the count relock() takes.
    pub fn unlock(self) -> SeqMutexSeq {
        SeqMutexSeq(self.lock().raw().seq.load(Ordering::Relaxed))
    }
}
