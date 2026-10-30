// SPDX-License-Identifier: GPL-2.0

//! A futex: sleep until a word changes from a value we saw, woken by the
//! word's address. The kernel's are for userspace addresses and aren't
//! exported, and its syscall implements only 32 bit ones; this is
//! bcachefs's, in Rust, the same in both builds, on a byte or a word.
//!
//! It's wait_event() without the waitlist on the fast path. A waiter reads
//! the word, checks its condition, and only if it's going to sleep calls
//! futex_wait() with the value it read - which compares it again under the
//! bucket lock, so a change in between, and the wake that came with it,
//! isn't lost: it returns at once. A lock waits on its own word (ByteLock);
//! an eventcount is a counter every wake bumps.
//!
//! Waiters queue in a static table of buckets, hashed by address, as the
//! kernel's futexes and wait_var_event() do. futex_wake() takes a bucket's
//! lock only if its count says someone's waiting - the kernel futex's
//! ordering (kernel/futex/waitwake.c). The waiter increments the count,
//! fences, reads the word; the waker writes the word, fences, reads the
//! count. One sees the other's write: the waiter sees the new value and
//! doesn't sleep, or the waker sees the waiter.
//!
//! An address is hashed and compared, never dereferenced, so waking one
//! whose memory is gone is fine: whatever's there now is woken spuriously,
//! and every waiter rechecks.
//!
//! A waiter's entry is on its stack, on its bucket's list while it sleeps,
//! and holds the waiting thread, as a wait_queue_entry's private does. The
//! waker takes it off, marks it woken and wakes the thread, all under the
//! bucket lock, as kernel/wait.c's wake_up() does. The waiter, woken or not -
//! a signal, a timeout - takes the lock once more on its way out, and takes
//! its entry off if it's still on: so the waker is done with it, and with the
//! thread, before it goes - no ref on the thread - and a wake meant for one
//! waiter that it took is a wake it reports.

use crate::c;
use crate::list;
use crate::locking::{spinlock_t, Lock};
use core::sync::atomic::{fence, AtomicBool, AtomicU32, AtomicU8, AtomicUsize, Ordering};
use crate::util::list::ListLinks;
use crate::util::sleep::{sleep_until, Sleep, SleepEnd};
use core::ptr;
use cstruct_macros::c_export;

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
struct Bucket {
    waiters: AtomicUsize,
    list:    Lock<spinlock_t, list!(Entry, links)>,
}

struct Table([Bucket; 1 << BUCKET_BITS]);

// Zeroed, until bch2_futex_init() - before anything can wait - makes each
// bucket's lock a lock and its list a list.
static TABLE: Table = unsafe { core::mem::zeroed() };

fn bucket(addr: usize) -> &'static Bucket {
    // hash_ptr(): times the golden ratio, the top bits
    let h = (addr as u64).wrapping_mul(0x61c8864680b583eb) >> (64 - BUCKET_BITS);
    &TABLE.0[h as usize]
}

/// Before anything can wait: at module init, in both builds.
#[c_export]
pub fn bch2_futex_init() {
    #[cfg(kernel)]
    let key = ::kernel::static_lock_class!().as_ptr().cast();
    #[cfg(not(kernel))]
    let key = ptr::null_mut();

    for b in &TABLE.0 {
        // SAFETY: zeroed, and nothing can take it until we return; static
        unsafe {
            let Ok(()) = b.list.init_zeroed(c"futex bucket", key);
            b.list.lock().init();
        }
    }
}

struct Entry {
    links:  ListLinks,
    addr:   usize,
    /// Off the list by the waker, under the lock
    woken:  AtomicBool,
    task:   *mut c::task_struct,
}

// SAFETY: the task is woken only under the bucket lock, which its waiter
// takes before it returns
unsafe impl Sync for Entry {}

/// Sleep, as @how says, while *@w == @expected, until a futex_wake() on @w.
/// Returns at once if it isn't equal; may return spuriously - the caller
/// checks its condition again either way.
pub fn futex_wait<W: FutexWord, E: SleepEnd>(w: &W, expected: W::Val, how: Sleep<E>) -> Result<(), E> {
    futex_wait_or(w, expected, how, || false)
}

/// futex_wait(), returning too on any wake that finds @woken(): for a waiter
/// whose condition isn't all futex words - kthread_should_stop(), which
/// kthread_stop() follows with a wake_up_process(), not a futex_wake(), or
/// whatever else C changes and wakes a task for. @woken runs in the sleep's
/// task state, so it mustn't sleep.
///
/// A waiter on several futex words - two EventCounts - would want a
/// vectored wait instead, its entry on each word's bucket: none does yet.
pub fn futex_wait_or<W: FutexWord, E: SleepEnd>(w: &W, expected: W::Val, how: Sleep<E>,
                                                mut woken: impl FnMut() -> bool) -> Result<(), E> {
    let addr = ptr::from_ref(w).addr();
    let b = bucket(addr);

    b.waiters.fetch_add(1, Ordering::Relaxed);
    fence(Ordering::SeqCst);

    let task = unsafe { c::bch2_current() };
    assert!(!task.is_null(), "a blocking wait needs a task - a kthread, in userspace");
    let entry = Entry {
        links:  ListLinks::new(),
        addr,
        woken:  AtomicBool::new(false),
        task,
    };

    let mut ret = Ok(());
    let mut list = b.list.lock();
    if w.load(Ordering::Relaxed) == expected {
        // SAFETY: init()ed; the entry's on no list, and is taken off below,
        // with the lock held, before it goes
        unsafe { list.push_back(&entry) };
        drop(list);

        ret = sleep_until(how, || (entry.woken.load(Ordering::Acquire) || woken()).then_some(()));

        // woken or not, the lock again: so the waker's done with it
        list = b.list.lock();
        // SAFETY: if it's on a list, it's this one; if it's not, it was woken
        if !unsafe { list.remove(&entry) } {
            ret = Ok(());
        }
    }
    drop(list);

    b.waiters.fetch_sub(1, Ordering::Relaxed);
    ret
}

/// Wake up to @n waiters on @addr; how many it woke. From any context, an
/// endio's included.
pub fn futex_wake(addr: *const (), n: usize) -> usize {
    fence(Ordering::SeqCst);

    let addr = addr.addr();
    let b = bucket(addr);
    if b.waiters.load(Ordering::Relaxed) == 0 {
        return 0;
    }

    let mut woken = 0;
    let mut list = b.list.lock();
    let mut cur = list.cursor_mut();
    while woken < n {
        let Some(e) = cur.current() else { break };
        if e.addr != addr {
            cur.move_next();
            continue;
        }
        cur.remove_current();
        e.woken.store(true, Ordering::Release);
        unsafe { c::wake_up_process(e.task) };
        woken += 1;
    }
    woken
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

    /// Sleep until @cond: checked after every wake(), and every other wake
    /// too, as wait_event() does - so @cond may be over what isn't this
    /// count, kthread_should_stop() say. It runs in the sleep's task state
    /// as well, so it mustn't sleep, and more than once a wake.
    pub fn wait_until<T, E: SleepEnd + Copy>(&self, how: Sleep<E>,
                                             mut cond: impl FnMut() -> Option<T>)
        -> Result<T, E>
    {
        loop {
            let seen = self.snapshot();
            if let Some(v) = cond() {
                return Ok(v);
            }
            futex_wait_or(&self.0, seen, how, || cond().is_some())?;
        }
    }

    pub fn wake(&self) {
        self.0.fetch_add(1, Ordering::Release);
        futex_wake(ptr::from_ref(&self.0).cast(), usize::MAX);
    }
}
