// SPDX-License-Identifier: GPL-2.0

//! A waitlist: each waiter's completion, in the waiter's own memory - a
//! WaitlistCompletion, linked while it waits, so a wait allocates nothing and
//! can begin anywhere: under RCU, on what RCU keeps. Any completion that's
//! Complete - a thread's Done, a task's, a Select's input - each completed
//! through its own type's complete().
//!
//! A waiter goes on the list, then checks its condition; a waker changes it,
//! then wakes. A full fence in each, so one sees the other: the waiter the
//! change, or the waker the waiter - kernel/futex/waitwake.c's ordering, by
//! which a wake takes the lock only if a count says someone's waiting.
//!
//! A wake takes its waiters off under the lock, onto a list of its own, and
//! completes them once it's dropped it: a completion may run a task's next
//! step, which may take the lock. A waiter going without a wake cancels,
//! taking its own off; if a wake took it first, the completion is the wake's,
//! and the waiter waits for it - the wake's last touch of its memory.

use crate::data_structures::list::{List, ListLinks};
use crate::locking::{Locked, SpinLockIrq};
use crate::os::lockdep::LockClass;
use crate::util::task::{Completion, Done, DynDone, Waiter};
use core::cell::{Cell, UnsafeCell};
use core::ffi::CStr;
use core::marker::PhantomPinned;
use core::pin::Pin;
use core::ptr::NonNull;
use core::sync::atomic::{fence, AtomicBool, AtomicUsize, Ordering};

/// What a waitlist completes, in place, from any context - an endio's too.
/// Seen done, its waiter's memory may go.
pub trait Complete: Sync {
    /// # Safety
    /// Once, while it's owed.
    unsafe fn complete(&self);

    fn done(&self) -> bool;
}

impl<W: Waiter> Complete for Done<W> {
    unsafe fn complete(&self) {
        unsafe { Done::complete(self) }
    }

    fn done(&self) -> bool {
        Done::done(self)
    }
}

/// A completer, completed in place: a Select's input.
pub struct DynComplete<'a> {
    done:  UnsafeCell<Option<DynDone<'a>>>,
    taken: AtomicBool,
}

// its completer is taken once, by complete()
unsafe impl Sync for DynComplete<'_> {}

impl<'a> DynComplete<'a> {
    pub fn new(done: DynDone<'a>) -> Self {
        DynComplete { done: UnsafeCell::new(Some(done)), taken: AtomicBool::new(false) }
    }
}

impl Complete for DynComplete<'_> {
    unsafe fn complete(&self) {
        let done = unsafe { (*self.done.get()).take() }.expect("a DynComplete completed twice");
        self.taken.store(true, Ordering::Release);
        done.complete();
    }

    fn done(&self) -> bool {
        self.taken.load(Ordering::Acquire)
    }
}

/* What the list sees of a WaitlistCompletion, whatever its completion */
#[repr(C)]
struct Waiting<K> {
    links:    ListLinks,
    key:      K,
    /* set under the list's lock by the wake that took it off */
    taken:    Cell<bool>,
    complete: unsafe fn(NonNull<Waiting<K>>),
}

// taken is the list lock's; complete() is Complete's, which is Sync
unsafe impl<K: Sync> Sync for Waiting<K> {}

pub struct WaitList<K> {
    waiters: AtomicUsize,
    list:    Locked<SpinLockIrq, List<Waiting<K>, 0>>,
}

impl<K> WaitList<K> {
    /// # Safety
    /// As Locked::init_zeroed()'s.
    pub unsafe fn init_zeroed(&self, name: &'static CStr, key: LockClass) {
        let Ok(()) = unsafe { self.list.init_zeroed(name, key) };
    }

    /// Up to @n of the waiters @picks, oldest first, completed: how many. For
    /// a change made before, that they check after they're on.
    pub fn wake(&self, n: usize, picks: impl Fn(&K) -> bool) -> usize {
        fence(Ordering::SeqCst);
        if self.waiters.load(Ordering::Relaxed) == 0 {
            return 0;
        }

        let mut taken = List::<Waiting<K>, 0>::new();
        let mut woken = 0;
        {
            let mut list = self.list.lock();
            let mut cur = list.cursor_mut();
            while woken < n {
                let Some(w) = cur.current() else { break };
                if !picks(&w.key) {
                    cur.move_next();
                    continue;
                }
                cur.remove_current();
                w.taken.set(true);
                unsafe { taken.push_back(w) };
                woken += 1;
            }
        }
        self.waiters.fetch_sub(woken, Ordering::Relaxed);

        while let Some(w) = taken.cursor_mut().remove_current() {
            unsafe { (w.complete)(NonNull::from(w)) };
        }
        woken
    }
}

/// A wait on a WaitList: its entry, and its completion, @T - completed by a
/// wake, or by its own cancel(). In its waiter's memory, pinned, it's waited
/// on to its end before it goes.
#[repr(C)]
pub struct WaitlistCompletion<'a, K, T: Complete> {
    w:    Waiting<K>,
    list: &'a WaitList<K>,
    done: T,
    _pin: PhantomPinned,
}

impl<'a, K, T: Complete> WaitlistCompletion<'a, K, T> {
    /// @done, on @list under @key, in @slot - then @ready() checked: true,
    /// what it waits for may have come before it was on, and it's cancelled.
    pub fn new<'s>(mut slot: Pin<&'s mut Option<Self>>, list: &'a WaitList<K>, key: K, done: T,
                   ready: impl FnOnce() -> bool) -> &'s Self
    {
        unsafe fn complete<K, T: Complete>(w: NonNull<Waiting<K>>) {
            unsafe { (*w.cast::<WaitlistCompletion<'_, K, T>>().as_ptr()).done.complete() }
        }

        slot.set(Some(WaitlistCompletion {
            w: Waiting { links: ListLinks::new(), key, taken: Cell::new(false), complete: complete::<K, T> },
            list,
            done,
            _pin: PhantomPinned,
        }));
        let wait = slot.into_ref().get_ref().as_ref().expect("just set");

        list.waiters.fetch_add(1, Ordering::Relaxed);
        fence(Ordering::SeqCst);
        unsafe { list.list.lock().push_back(&wait.w) };
        if ready() {
            wait.cancel();
        }
        wait
    }

    /// Off its list and completed, unless a wake took it first - which
    /// completes it: whether it was still on. Either way, it's then waited on.
    pub fn cancel(&self) -> bool {
        let on = {
            let mut list = self.list.list.lock();
            !self.w.taken.get() && unsafe { list.remove(&self.w) }
        };
        if on {
            self.list.waiters.fetch_sub(1, Ordering::Relaxed);
            unsafe { self.done.complete() };
        }
        on
    }

    pub fn is_done(&self) -> bool {
        self.done.done()
    }
}

impl<'a, K, W: Waiter> WaitlistCompletion<'a, K, Done<W>> {
    /// new(), completing a Done born with @waiter.
    pub fn waiter<'s>(slot: Pin<&'s mut Option<Self>>, list: &'a WaitList<K>, key: K, waiter: W,
                      ready: impl FnOnce() -> bool) -> &'s Self
    {
        Self::new(slot, list, key, Done::new(waiter), ready)
    }

    pub fn completion(&self) -> Completion<'_, W> {
        Completion::owed(&self.done)
    }
}

impl<K, T: Complete> Drop for WaitlistCompletion<'_, K, T> {
    fn drop(&mut self) {
        self.cancel();
        assert!(self.done.done(), "a waitlist wait dropped before it was done: wait on it, or await it, first");
    }
}
