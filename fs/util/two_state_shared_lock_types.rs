// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{c_default};
use crate::util::alloc::init_at;
use crate::util::futex::EventCount;
use crate::util::sleep::Sleep;
use cstruct_macros::MaybeZeroable;
use core::marker::PhantomData;
use core::sync::atomic::AtomicIsize;
use core::sync::atomic::Ordering::{Acquire, Relaxed, Release};

/*
 * Two-state lock - can be taken for add or block - both states are shared,
 * like read side of rwsem, but conflict with other state:
 */
#[repr(C)]
#[derive(MaybeZeroable)]
pub struct two_state_lock_t {
    pub v: AtomicIsize,
    /// Woken as the last holder unlocks
    wait: EventCount,
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

    /// Held in @s without a guard: for an owner that keeps it past a borrow -
    /// a DIO write's DioStarted - and unlocks it with unlock_raw().
    pub(crate) fn lock_raw(&self, s: bool) {
        loop {
            let seen = self.wait.snapshot();
            if self.trylock(s) {
                return;
            }
            let Ok(()) = self.wait.wait(seen, Sleep::UNINTERRUPTIBLE);
        }
    }

    pub(crate) fn unlock_raw(&self, s: bool) {
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
        // then(), not then_some(): a guard built for a failed trylock would
        // unlock on drop
        self.trylock(S::S).then(|| TwoStateGuard { lock: self, _s: PhantomData })
    }

    /// @f with the lock dropped, and taken again after.
    ///
    /// # Safety
    /// Our caller holds it in @S - not a guard we have.
    pub unsafe fn unlocked_while<S: TwoState, R>(&self, f: impl FnOnce() -> R) -> R {
        self.unlock_raw(S::S);
        let r = f();
        self.lock_raw(S::S);
        r
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
pub unsafe fn two_state_lock_init(lock: *mut c::two_state_lock_t) {
    unsafe {
        (&raw mut (*lock).v).write(AtomicIsize::new(0));
        let Ok(()) = init_at(EventCount::new(), &raw mut (*lock).wait);
    }
}
