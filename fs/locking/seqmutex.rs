// SPDX-License-Identifier: GPL-2.0

//! A seqmutex: a mutex, and a count of the times it's been taken - so a
//! holder can drop it and take it back only if no one else took it
//! meanwhile, relock(). C's struct, with C's API in seqmutex.h. C reads the
//! count plainly, outside the lock; Rust, as an atomic.

#![allow(non_camel_case_types)]

use super::{Guard, Lock, RawLock};
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
    pub seq:  u32,
}
c_default!(seqmutex);

/// @l's count: a u32, aligned as one, that C writes under the mutex.
unsafe fn seq<'a>(l: *mut seqmutex) -> &'a AtomicU32 {
    unsafe { AtomicU32::from_ptr(&raw mut (*l).seq) }
}

unsafe impl RawLock for seqmutex {
    type GuardState = ();
    type InitError = Infallible;

    unsafe fn init(l: *mut Self, name: &'static CStr, key: *mut c_void) -> Result<(), Infallible> {
        unsafe {
            (&raw mut (*l).seq).write(0);
            <c::mutex as RawLock>::init(&raw mut (*l).lock, name, key)
        }
    }

    unsafe fn lock(l: *mut Self) {
        unsafe {
            <c::mutex as RawLock>::lock(&raw mut (*l).lock);
            seq(l).fetch_add(1, Ordering::Relaxed);
        }
    }

    unsafe fn unlock(l: *mut Self, _: ()) {
        unsafe { <c::mutex as RawLock>::unlock(&raw mut (*l).lock, ()) }
    }

    unsafe fn assert_held(l: *mut Self) {
        unsafe { <c::mutex as RawLock>::assert_held(&raw mut (*l).lock) }
    }
}

/// What relock() takes: the count at an unlock.
pub struct SeqMutexSeq(u32);

impl<G> Lock<seqmutex, G> {
    /// Held again, if no one has taken it since the unlock that gave @at.
    pub fn relock(&self, at: SeqMutexSeq) -> Option<Guard<'_, seqmutex, G>> {
        let l = self.raw();
        if unsafe { seq(l) }.load(Ordering::Relaxed) != at.0 {
            return None;
        }

        if unsafe { c::mutex_trylock((&raw mut (*l).lock).cast()) } as i32 == 0 {
            return None;
        }
        let g = unsafe { Guard::from_held(self, Some(())) };

        if unsafe { seq(l) }.load(Ordering::Relaxed) != at.0 {
            return None;    // unlocked, as the guard drops
        }
        Some(g)
    }
}

impl<G> Guard<'_, seqmutex, G> {
    /// Released, with the count relock() takes.
    pub fn unlock(self) -> SeqMutexSeq {
        SeqMutexSeq(unsafe { seq(self.lock().raw()) }.load(Ordering::Relaxed))
    }
}
