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
//! A waiter's entry is on its stack - or in its future, for a task - on its
//! bucket's list while it waits, and holds its waiter's DynDone: a thread's
//! Done or a task's, as a wait_queue_entry's private holds its thread. The waker takes
//! it off and completes the Done, under the bucket lock, as kernel/wait.c's
//! wake_up() does. The waiter, woken or not - a signal, a timeout, a dropped
//! future - takes the lock once more on its way out, and takes its entry off
//! if it's still on, completing its own Done: so the waker is done with it
//! before it goes, and a wake meant for one waiter that it took is a wake it
//! reports.

use crate::list;
use crate::locking::{Locked, SpinLockIrq};
use core::sync::atomic::{fence, AtomicU32, AtomicU8, AtomicUsize, Ordering};
use crate::data_structures::list::ListLinks;
use crate::os::kernel::jiffies;
use crate::util::sleep::{sleep_until, Sleep, SleepEnd, SleepError};
use crate::util::task::{waiter, Completion, Done, DynDone, Resumable, Resume, Select};
use crate::util::timer::Timer;
use core::cell::UnsafeCell;
use core::ffi::c_ulong;
use core::marker::PhantomPinned;
use core::pin::{pin, Pin};
use core::ptr;
use core::task::{Context, Poll};
use pin_init::{pin_data, pinned_drop, stack_pin_init};

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
    list:    Locked<SpinLockIrq, list!(Entry, links)>,
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
pub fn bch2_futex_init() {
    let key = crate::lock_class_key!();

    for b in &TABLE.0 {
        // SAFETY: zeroed, and nothing can take it until we return; static
        unsafe {
            let Ok(()) = b.list.init_zeroed(c"futex bucket", key);
            b.list.lock().init();
        }
    }
}

struct Entry {
    links: ListLinks,
    addr:  usize,
    /// Taken by the waker, off the list, under the lock - or, with no wake,
    /// completed as the entry goes
    done:  UnsafeCell<Option<DynDone<'static>>>,
}

// SAFETY: its Done is taken only by the waker, under the bucket lock, or
// dropped with it once dequeue() has taken it off under that lock
unsafe impl Send for Entry {}
unsafe impl Sync for Entry {}

impl Entry {
    /// For a wait on @w: its Done, completed as it goes, if no wake takes it.
    fn new<W: FutexWord>(w: &W, done: DynDone<'static>) -> Self {
        Entry { links: ListLinks::new(), addr: ptr::from_ref(w).addr(), done: UnsafeCell::new(Some(done)) }
    }

    /// On its bucket's list if *@w is still @expected - read under the lock,
    /// after counting it a waiter, for futex_wake()'s ordering - and counted
    /// till it's dequeue()d; or not, and not counted.
    ///
    /// # Safety
    /// Queued, it's dequeue()d before it moves or goes.
    unsafe fn queue<W: FutexWord>(&self, w: &W, expected: W::Val) -> bool {
        let b = bucket(self.addr);
        b.waiters.fetch_add(1, Ordering::Relaxed);
        fence(Ordering::SeqCst);

        let mut list = b.list.lock();
        let queued = w.load(Ordering::Relaxed) == expected;
        if queued {
            unsafe { list.push_back(self) };
        }
        drop(list);

        if !queued {
            b.waiters.fetch_sub(1, Ordering::Relaxed);
        }
        queued
    }

    /// Off its bucket's list, unless a wake took it first - woken or not, the
    /// lock again: so the waker's done with it - and not counted a waiter:
    /// whether it was still on. Its Done is then completed as it goes.
    fn dequeue(&self) -> bool {
        let b = bucket(self.addr);
        // if it's on a list, it's this one
        let on = unsafe { b.list.lock().remove(self) };
        b.waiters.fetch_sub(1, Ordering::Relaxed);
        on
    }

    /// Woken: taken off by the waker, under the lock - its Done completed.
    ///
    /// # Safety
    /// Under the bucket lock, just taken off the list.
    unsafe fn wake(&self) {
        unsafe { (*self.done.get()).take() }.expect("a futex waiter woken twice").complete();
    }
}

/// Sleep, as @how says, while *@w == @expected, until a futex_wake() on @w.
/// Returns at once if it isn't equal; may return spuriously - the caller
/// checks its condition again either way.
pub fn futex_wait<W: FutexWord, E: SleepEnd>(w: &W, expected: W::Val, how: Sleep<E>) -> Result<(), E> {
    let mut done = Done::completed();
    let (c, d) = Completion::new_dyn(&mut done, waiter::Sync::current());
    // declared after @c, so dropped before it: its Done completed first
    let entry = Entry::new(w, d);

    if !unsafe { entry.queue(w, expected) } {
        return Ok(());
    }

    let ret = sleep_until(how, || c.is_done().then_some(()));
    // a wake meant for one waiter that we took is a wake we report
    if entry.dequeue() { ret } else { Ok(()) }
}

/// futex_wait(), for a task: parked while *@w == @expected, until a
/// futex_wake() on @w, and resumed @at. As futex_wait(), it may return
/// spuriously.
pub async fn futex_wait_async<W: FutexWord + Sync>(w: &W, expected: W::Val, at: Resume<'_>)
    where W::Val: Send
{
    let mut done = Done::completed();
    let (c, d) = Completion::new_dyn(&mut done, waiter::Async::current().await);
    FutexWait::new(w, expected, d, c).resume_on(at).await;
}

/// futex_wait_async(), for at most @timeout jiffies: a Select of the wake and
/// a Timer - Err(Timeout) if the timer ended it, and no wake took the entry.
pub async fn futex_wait_async_for<W: FutexWord + Sync>(w: &W, expected: W::Val, at: Resume<'_>,
                                                      timeout: c_ulong) -> Result<(), SleepError>
    where W::Val: Send
{
    let mut slot = pin!(None);
    let (mut wait, [wake, timed_out]) = Select::new(slot.as_mut(), waiter::Async::current().await);
    stack_pin_init!(let _timer = Timer::new(timed_out, timeout));

    let woken = FutexWait::new(w, expected, wake, wait.next()).resume_on(at).await;
    if woken { Ok(()) } else { Err(SleepError::Timeout) }
}

/// futex_wait_async()'s wait: its entry queued by its first poll, once it's
/// pinned, then @c awaited - what the entry's Done completes, or a Select of
/// it - and the entry taken off: whether a wake took it first, or the word
/// had changed. Dropped before then, it takes the entry off, whose Done is
/// completed as it goes - before @c's drop, which would wait for it.
#[pin_data(PinnedDrop)]
struct FutexWait<'w, W: FutexWord, C> {
    w:        &'w W,
    expected: W::Val,
    /* before @c: dropped first */
    entry:    Entry,
    #[pin]
    c:        C,
    /// Polled: whether the entry's on the list - None till then
    queued:   Option<bool>,
    #[pin]
    _pin:     PhantomPinned,
}

impl<'w, W: FutexWord, C: Resumable> FutexWait<'w, W, C> {
    /// Its entry completing @done, which @c awaits.
    fn new(w: &'w W, expected: W::Val, done: DynDone<'w>, c: C) -> Self {
        let done = unsafe { done.erase() };
        FutexWait { w, expected, entry: Entry::new(w, done), c, queued: None, _pin: PhantomPinned }
    }
}

impl<W: FutexWord, C: Resumable> Resumable for FutexWait<'_, W, C> {
    /// Woken: a wake took the entry, or there was none to wait for
    type Output = bool;

    fn poll_resume(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<bool> {
        let this = self.project();

        if this.queued.is_none() {
            // pinned: dequeue()d by the drop
            let queued = unsafe { this.entry.queue(*this.w, *this.expected) };
            *this.queued = Some(queued);
            if !queued {
                return Poll::Ready(true);
            }
        }

        core::task::ready!(this.c.poll_resume(cx));
        // a wake meant for one waiter that we took is a wake we report
        *this.queued = Some(false);
        Poll::Ready(!this.entry.dequeue())
    }
}

#[pinned_drop]
impl<W: FutexWord, C> PinnedDrop for FutexWait<'_, W, C> {
    fn drop(self: Pin<&mut Self>) {
        let this = self.project();
        if *this.queued == Some(true) {
            this.entry.dequeue();
        }
    }
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
        unsafe { e.wake() };
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

    /// wait_async(), for at most @timeout jiffies.
    pub async fn wait_async_for(&self, seen: u32, at: Resume<'_>, timeout: c_ulong) -> Result<(), SleepError> {
        futex_wait_async_for(&self.0, seen, at, timeout).await
    }

    pub fn wake(&self) {
        self.0.fetch_add(1, Ordering::Release);
        futex_wake(ptr::from_ref(&self.0).cast(), usize::MAX);
    }
}
