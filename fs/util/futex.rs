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
//! A waiter's entry is on its stack, on its bucket's list while it sleeps.
//! The waker takes it off and marks it woken, under the bucket lock, and
//! wakes it once the lock is dropped. The waiter, woken or not - a signal, a
//! timeout - takes the lock once more on its way out, and takes its entry off
//! if it's still on: so the waker is done with it before it goes, as List
//! requires, and a wake meant for one waiter that it took is a wake it
//! reports.
//!
//! The waiter is a Waker: for now always a blocking wait's task_waker(). A
//! task awaiting would queue an entry holding its own, woken the same way.

use crate::list;
use crate::locking::{spinlock_t, Lock};
use core::sync::atomic::{fence, AtomicBool, AtomicU32, AtomicU8, AtomicUsize, Ordering};
use crate::util::list::ListLinks;
use crate::util::sleep::{sleep_until, task_waker, Sleep, SleepEnd};
use core::cell::UnsafeCell;
use core::ptr;
use core::task::Waker;
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
    /// Off the list by the waker: set last, under the lock
    woken:  AtomicBool,
    /// Whoever takes the entry off the list owns it
    waker:  UnsafeCell<Option<Waker>>,
}

// SAFETY: its waker is touched only by whoever took it off the list - once
unsafe impl Sync for Entry {}

/// Sleep, as @how says, while *@w == @expected, until a futex_wake() on @w.
/// Returns at once if it isn't equal; may return spuriously - the caller
/// checks its condition again either way.
pub fn futex_wait<W: FutexWord, E: SleepEnd>(w: &W, expected: W::Val, how: Sleep<E>) -> Result<(), E> {
    let addr = ptr::from_ref(w).addr();
    let b = bucket(addr);

    b.waiters.fetch_add(1, Ordering::Relaxed);
    fence(Ordering::SeqCst);

    let entry = Entry {
        links:  ListLinks::new(),
        addr,
        woken:  AtomicBool::new(false),
        waker:  UnsafeCell::new(Some(task_waker())),
    };

    let mut ret = Ok(());
    let mut list = b.list.lock();
    if w.load(Ordering::Relaxed) == expected {
        // SAFETY: init()ed; the entry's on no list, and is taken off below,
        // with the lock held, before it goes
        unsafe { list.push_back(&entry) };
        drop(list);

        ret = sleep_until(how, || entry.woken.load(Ordering::Acquire).then_some(()));

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

    // Woken a batch at a time, after the lock's dropped
    const BATCH: usize = 8;
    let mut woken = 0;
    loop {
        let mut batch: [Option<Waker>; BATCH] = [const { None }; BATCH];
        let mut nr = 0;
        let mut more = false;
        {
            let mut list = b.list.lock();
            let mut c = list.cursor_mut();
            while let Some(e) = c.current() {
                if nr == BATCH || woken + nr == n {
                    more = woken + nr < n;
                    break;
                }
                if e.addr != addr {
                    c.move_next();
                    continue;
                }
                c.remove_current();
                // SAFETY: we took it off: its waker is ours
                batch[nr] = unsafe { (*e.waker.get()).take() };
                e.woken.store(true, Ordering::Release);
                nr += 1;
            }
        }

        for w in batch.iter_mut().take(nr) {
            w.take().expect("a queued futex waiter without its Waker").wake();
        }
        woken += nr;

        if !more {
            return woken;
        }
    }
}

/// wait_event(), on a futex: for a condition over state that isn't a futex
/// word. Every change to the state is followed by a wake(), which bumps the
/// count; a waiter reads the count before checking its condition, so a
/// change after the check leaves the count moved and the wait returns.
pub struct EventCount(AtomicU32);

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

    pub fn wait_until<T, E: SleepEnd + Copy>(&self, how: Sleep<E>,
                                             mut cond: impl FnMut() -> Option<T>)
        -> Result<T, E>
    {
        loop {
            let seen = self.snapshot();
            if let Some(v) = cond() {
                return Ok(v);
            }
            self.wait(seen, how)?;
        }
    }

    pub fn wake(&self) {
        self.0.fetch_add(1, Ordering::Release);
        futex_wake(ptr::from_ref(&self.0).cast(), usize::MAX);
    }
}
