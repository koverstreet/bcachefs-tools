// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{c_default, rust_only, RustOnly};
use crate::util::alloc::init_at;
use crate::util::futex::EventCount;
use crate::util::sleep::Sleep;
use cstruct_macros::{c_export, CStruct};
use core::ffi::c_int;
use core::marker::PhantomData;
use core::sync::atomic::AtomicIsize;
use core::sync::atomic::Ordering::{Acquire, Relaxed, Release};

/*
 * Two-state lock - can be taken for add or block - both states are shared,
 * like read side of rwsem, but conflict with other state:
 */
#[repr(C)]
#[derive(CStruct)]
#[c_typedef]
pub struct two_state_lock_t {
    pub v: AtomicIsize,
    /// Woken as the last holder unlocks
    wait: rust_only!(EventCount),
}
c_default!(two_state_lock_t);

fn delta(s: bool) -> isize {
    if s { 1 } else { -1 }
}

/// One of a two-state lock's states, as its user names it.
pub trait TwoState {
    const S: bool;
}

impl two_state_lock_t {
    fn trylock(&self, s: bool) -> bool {
        let i = delta(s);
        let mut old = self.v.load(Relaxed);
        loop {
            if if i > 0 { old < 0 } else { old > 0 } {
                return false;
            }
            match self.v.compare_exchange_weak(old, old + i, Acquire, Relaxed) {
                Ok(_) => return true,
                Err(v) => old = v,
            }
        }
    }

    fn lock_raw(&self, s: bool) {
        loop {
            let seen = self.wait.snapshot();
            if self.trylock(s) {
                return;
            }
            let Ok(()) = self.wait.wait(seen, Sleep::UNINTERRUPTIBLE);
        }
    }

    fn unlock_raw(&self, s: bool) {
        debug_assert!(self.v.load(Relaxed) != 0, "a two-state lock unlocked, not held");
        let i = delta(s);
        if self.v.fetch_sub(i, Release) == i {
            self.wait.wake();
        }
    }

    pub fn lock<S: TwoState>(&self) -> TwoStateGuard<'_, S> {
        self.lock_raw(S::S);
        TwoStateGuard { lock: self, _s: PhantomData }
    }

    pub fn try_lock<S: TwoState>(&self) -> Option<TwoStateGuard<'_, S>> {
        self.trylock(S::S).then_some(TwoStateGuard { lock: self, _s: PhantomData })
    }
}

pub struct TwoStateGuard<'a, S: TwoState> {
    lock: &'a two_state_lock_t,
    _s:   PhantomData<S>,
}

impl<S: TwoState> Drop for TwoStateGuard<'_, S> {
    fn drop(&mut self) {
        self.lock.unlock_raw(S::S);
    }
}

/// # Safety
/// @lock is a two_state_lock_t's memory, not yet in use.
#[c_export]
pub unsafe fn two_state_lock_init(lock: *mut c::two_state_lock_t) {
    unsafe {
        (&raw mut (*lock).v).write(AtomicIsize::new(0));
        let Ok(()) = init_at(RustOnly::new(EventCount::new()), &raw mut (*lock).wait);
    }
}

#[c_export]
pub fn bch2_two_state_trylock(lock: &c::two_state_lock_t, s: c_int) -> bool {
    lock.trylock(s != 0)
}

#[c_export]
pub fn bch2_two_state_lock(lock: &c::two_state_lock_t, s: c_int) {
    lock.lock_raw(s != 0)
}

#[c_export]
pub fn bch2_two_state_unlock(lock: &c::two_state_lock_t, s: c_int) {
    lock.unlock_raw(s != 0)
}
