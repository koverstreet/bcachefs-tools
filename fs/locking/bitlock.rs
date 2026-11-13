// SPDX-License-Identifier: GPL-2.0

//! Waiting on bits of a word: C's wait_on_bit() and wake_up_bit(), on
//! util/futex.rs, as ByteLock is - the same in both builds, where C's are the
//! kernel's bit waitqueues and the shim's.
//!
//! A waiter sleeps on the word's address, so every bit of a word shares one
//! wait: a wake wakes them all, and each rechecks its condition - of the whole
//! word, so a waiter can wait on several bits at once. The waker changes the
//! word first, then wakes; futex_wake() fences between the two, as a
//! waiter's futex_wait() does between its count and its read.

use crate::util::futex::{futex_wait, futex_wake, FutexWord};
use crate::os::kernel::jiffies;
use crate::util::sleep::{Sleep, SleepEnd};
use core::ffi::{c_long, c_ulong};
use core::sync::atomic::Ordering;

/// Sleep, as @how says, until @cond of @word holds - read with acquire, as
/// test_bit_acquire() is.
pub fn wait_until<W: FutexWord, E: SleepEnd>(word: &W, how: Sleep<E>, mut cond: impl FnMut(W::Val) -> bool)
    -> Result<(), E>
{
    loop {
        let v = word.load(Ordering::Acquire);
        if cond(v) {
            return Ok(());
        }
        futex_wait(word, v, how)?;
    }
}

/// wait_until(), given up after @timeout jiffies: whether @cond held.
pub fn wait_until_timeout<W: FutexWord>(word: &W, how: Sleep, timeout: c_ulong, mut cond: impl FnMut(W::Val) -> bool)
    -> bool
{
    let end = jiffies().wrapping_add(timeout);
    loop {
        let v = word.load(Ordering::Acquire);
        if cond(v) {
            return true;
        }
        let left = end.wrapping_sub(jiffies()) as c_long;
        if left <= 0 {
            return false;
        }
        let _ = futex_wait(word, v, how.timeout(left as c_ulong));
    }
}

/// Wake everyone waiting on @word - after changing what they wait for. From
/// any context, an endio's included.
pub fn wake<W>(word: &W) {
    futex_wake(core::ptr::from_ref(word).cast(), usize::MAX);
}
