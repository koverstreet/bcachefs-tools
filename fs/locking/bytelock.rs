// SPDX-License-Identifier: GPL-2.0

//! ByteLock: a sleeping lock in one byte, for what there are millions of and
//! no room in - a bucket's gc state, a gc stripe's. Not for contention: it
//! has no fairness and no lockdep.
//!
//! The futex mutex of Drepper's "Futexes Are Tricky", on util/futex.rs. The
//! byte is 0, unlocked; 1, locked; 2, locked and maybe waited on:
//!
//! - lock: 0 to 1. Taken: swap in 2, and futex_wait() while that swap doesn't
//!   get 0 - and when it does, it's ours, as 2, as there may be others.
//! - unlock: swap in 0, and futex_wake() one waiter if it was 2. Uncontended,
//!   lock and unlock are one atomic each.
//!
//! It sleeps, where a spinlock would do for how briefly it's held: holders
//! fsck_err(), which takes a mutex and may allocate.
//!
//! Zeroed is unlocked, with nothing to initialize: arrays of them are
//! allocated zeroed. So there's no lockdep class, and assert_held() can say
//! only that someone holds it.
//!
//! C had these as a bit in an unsigned long overlapping the struct's next
//! fields, because xchg() on a byte wasn't on every architecture. Rust's byte
//! atomics are: LLVM masks a word where the CPU has none, and a plain store
//! to the word's other bytes only makes the LL/SC or CAS retry. C, while it
//! still takes them, does so through bch2_byte_lock() and bch2_byte_unlock().

use super::{Lock, RawLock, RawTryLock};
use core::sync::atomic::{AtomicU8, Ordering};
use crate::util::futex::{futex_wait, futex_wake};
use crate::util::sleep::Sleep;
use core::convert::Infallible;
use core::ffi::{c_void, CStr};
use cstruct_macros::c_export;
use pin_init::Zeroable;

const UNLOCKED:  u8 = 0;
const LOCKED:    u8 = 1;
const CONTENDED: u8 = 2;

#[repr(transparent)]
pub struct ByteLock(AtomicU8);

// SAFETY: zeroed is unlocked
unsafe impl Zeroable for ByteLock {}
// SAFETY: zeroed, a Lock over a ByteLock is unlocked and initialized - if
// what it holds is valid zeroed
unsafe impl<G: Zeroable, P: Zeroable> Zeroable for Lock<ByteLock, G, P> {}

impl ByteLock {
    fn try_take(&self) -> bool {
        self.0.compare_exchange(UNLOCKED, LOCKED, Ordering::Acquire, Ordering::Relaxed).is_ok()
    }

    fn take(&self) {
        // as C's wait_on_bit_lock(): caught taken where it can't sleep, even
        // when it wouldn't
        #[cfg(kernel)]
        ::kernel::task::might_sleep();

        if !self.try_take() {
            self.take_contended();
        }
    }

    #[cold]
    fn take_contended(&self) {
        while self.0.swap(CONTENDED, Ordering::Acquire) != UNLOCKED {
            let Ok(()) = futex_wait(&self.0, CONTENDED, Sleep::UNINTERRUPTIBLE);
        }
    }

    fn release(&self) {
        if self.0.swap(UNLOCKED, Ordering::Release) == CONTENDED {
            futex_wake(core::ptr::from_ref(&self.0).cast(), 1);
        }
    }
}

unsafe impl RawLock for ByteLock {
    type GuardState = ();
    type InitError = Infallible;

    unsafe fn init(l: *mut Self, _name: &'static CStr, _key: *mut c_void) -> Result<(), Infallible> {
        unsafe { l.write(ByteLock(AtomicU8::new(UNLOCKED))) };
        Ok(())
    }

    unsafe fn lock(&self) {
        self.take()
    }

    unsafe fn unlock(&self, _: ()) {
        self.release()
    }

    unsafe fn assert_held(&self) {
        debug_assert_ne!(self.0.load(Ordering::Relaxed), UNLOCKED);
    }
}

unsafe impl RawTryLock for ByteLock {
    unsafe fn try_lock(&self) -> Option<()> {
        self.try_take().then_some(())
    }
}

// ── C's, while C takes them ──────────────────────────────────────────────

/// # Safety
/// @l is a ByteLock: a byte only ever locked as one, or zeroed.
#[c_export]
pub unsafe fn bch2_byte_lock(l: *mut u8) {
    unsafe { (*l.cast::<ByteLock>()).take() }
}

/// # Safety
/// @l is a ByteLock the caller holds.
#[c_export]
pub unsafe fn bch2_byte_unlock(l: *mut u8) {
    unsafe { (*l.cast::<ByteLock>()).release() }
}
