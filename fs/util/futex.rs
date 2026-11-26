// SPDX-License-Identifier: GPL-2.0

//! A futex: sleep until a word changes from a value we saw, woken by the
//! word's address. The kernel's are for userspace addresses and aren't
//! exported, and its syscall implements only 32 bit ones; this is
//! bcachefs's, in Rust, the same in both builds, on a byte or a word.
//!
//! It's wait_event() without the waitlist on the fast path. A waiter reads
//! the word, checks its condition, and only if it's going to sleep calls
//! futex_wait() with the value it read - which compares it again once it's
//! on its bucket's waitlist, so a change in between, and the wake that came
//! with it, isn't lost: it returns at once. A lock waits on its own word
//! (ByteLock); an eventcount is a counter every wake bumps.
//!
//! Waiters queue in a static table of buckets, hashed by address, as the
//! kernel's futexes and wait_var_event() do: each a WaitList, keyed by
//! address. The waiter's on its bucket's list, then reads the word; the
//! waker writes the word, then wakes - WaitList's ordering, the kernel
//! futex's: the waiter sees the new value, or the waker sees the waiter.
//!
//! An address is hashed and compared, never dereferenced, so waking one
//! whose memory is gone is fine: whatever's there now is woken spuriously,
//! and every waiter rechecks.
//!
//! A wait is a WaitlistCompletion, any waiter's, on its bucket's list: a
//! futex_wake() takes it off and completes it, or the waiter takes it off
//! and completes it itself. A waiter whose wait a wake took waits for the
//! wake's completion, and reports a wake.

use crate::os::kernel::jiffies;
use crate::util::sleep::{sleep_until, Sleep, SleepEnd, SleepError};
use crate::util::task::{waiter, Resumable, Resume, WaitList, WaitSync, WaitlistCompletion};
use crate::util::timer::with_timeout;
use core::pin::pin;
use core::ptr;
use core::sync::atomic::{AtomicU32, AtomicU8, AtomicUsize, Ordering};

/// A word futex_wait() can wait on.
pub trait FutexWord {
    type Val: Copy + Eq;
    fn load(&self, order: Ordering) -> Self::Val;
}

macro_rules! futex_word {
    ($($atomic:ty: $val:ty),*) => {
        $(impl FutexWord for $atomic {
            type Val = $val;
            fn load(&self, order: Ordering) -> $val {
                <$atomic>::load(self, order)
            }
        })*
    };
}

futex_word!(AtomicU8: u8, AtomicU32: u32, AtomicUsize: usize);

const BUCKET_BITS: u32 = 8;

#[repr(align(64))]
struct WaitBucket(WaitList<usize>);

struct Table([WaitBucket; 1 << BUCKET_BITS]);

// Zeroed, until futex_init() - before anything can wait - makes each
// bucket's lock a lock.
static TABLE: Table = unsafe { core::mem::zeroed() };

fn bucket(addr: usize) -> &'static WaitList<usize> {
    // hash_ptr(): times the golden ratio, the top bits
    let h = (addr as u64).wrapping_mul(0x61c8864680b583eb) >> (64 - BUCKET_BITS);
    &TABLE.0[h as usize].0
}

/// @w's bucket's list, and its key there.
fn waitlist<W: FutexWord>(w: &W) -> (&'static WaitList<usize>, usize) {
    let addr = ptr::from_ref(w).addr();
    (bucket(addr), addr)
}

/// Before anything can wait: at module init, in both builds.
pub fn futex_init() {
    let key = crate::lock_class_key!();

    for b in &TABLE.0 {
        // SAFETY: zeroed, and nothing can take it until we return; static
        unsafe { b.0.init_zeroed(c"futex bucket", key) };
    }
}

/// Sleep, as @how says, while *@w == @expected, until a futex_wake() on @w.
/// Returns at once if it isn't equal; may return spuriously - the caller
/// checks its condition again either way.
pub fn futex_wait<W: FutexWord, E: SleepEnd>(w: &W, expected: W::Val, how: Sleep<E>) -> Result<(), E> {
    let (list, addr) = waitlist(w);
    let mut slot = pin!(None);
    let wait = WaitlistCompletion::waiter(slot.as_mut(), list, addr, waiter::Sync::current(),
                                          || w.load(Ordering::Relaxed) != expected);

    let ret = sleep_until(how, || wait.is_done().then_some(()));
    let cancelled = wait.cancel();
    wait.completion().wait_sync(Sleep::IDLE);
    // a wake meant for one waiter that we took is a wake we report
    if cancelled { ret } else { Ok(()) }
}

/// futex_wait(), for a task: parked while *@w == @expected, until a
/// futex_wake() on @w, and resumed @at. As futex_wait(), it may return
/// spuriously.
pub async fn futex_wait_async<W: FutexWord + Sync>(w: &W, expected: W::Val, at: Resume<'_>)
    where W::Val: Send
{
    let (list, addr) = waitlist(w);
    let mut slot = pin!(None);
    let wait = WaitlistCompletion::waiter(slot.as_mut(), list, addr, waiter::Async::current().await,
                                          || w.load(Ordering::Relaxed) != expected);
    wait.completion().resume_on(at).await;
}

/// Wake up to @n waiters on @addr; how many it woke. From any context, an
/// endio's included.
pub fn futex_wake(addr: *const (), n: usize) -> usize {
    let addr = addr.addr();
    bucket(addr).wake(n, |&a| a == addr)
}

/// wait_event(), on a futex: for a condition over state that isn't a futex
/// word. Every change to the state is followed by a wake(), which bumps the
/// count; a waiter reads the count before checking its condition, so a
/// change after the check leaves the count moved and the wait returns.
#[repr(transparent)]
pub struct EventCount(AtomicU32);

// SAFETY: zeroed, it's new()'s
unsafe impl ::pin_init::Zeroable for EventCount {}

impl EventCount {
    pub const fn new() -> Self {
        EventCount(AtomicU32::new(0))
    }

    /// Read before checking the condition, for wait().
    pub fn snapshot(&self) -> u32 {
        self.0.load(Ordering::Acquire)
    }

    /// Sleep until a wake() after @seen - or at once, if there's been one.
    pub fn wait<E: SleepEnd>(&self, seen: u32, how: Sleep<E>) -> Result<(), E> {
        futex_wait(&self.0, seen, how)
    }

    /// Sleep until @cond, checked after every wake(): whatever it's over, a
    /// change to it must wake() this. @how's timeout is for the whole wait.
    pub fn wait_until<T, E: SleepEnd + Copy>(&self, how: Sleep<E>,
                                             mut cond: impl FnMut() -> Option<T>)
        -> Result<T, E>
    {
        let start = jiffies();
        loop {
            let seen = self.snapshot();
            if let Some(v) = cond() {
                return Ok(v);
            }
            self.wait(seen, how.since(start))?;
        }
    }

    /// wait(), for a task: parked until a wake() after @seen, resumed @at.
    pub async fn wait_async(&self, seen: u32, at: Resume<'_>) {
        futex_wait_async(&self.0, seen, at).await
    }

    /// wait_async(), for at most @timeout jiffies: Err(Timeout) if it ran out.
    pub async fn wait_async_timeout(&self, seen: u32, timeout: usize, at: Resume<'_>)
        -> Result<(), SleepError>
    {
        let (list, addr) = waitlist(&self.0);
        with_timeout(timeout, at, list, addr, || self.0.load(Ordering::Relaxed) != seen).await
    }

    pub fn wake(&self) {
        self.0.fetch_add(1, Ordering::Release);
        futex_wake(ptr::from_ref(&self.0).cast(), usize::MAX);
    }
}
