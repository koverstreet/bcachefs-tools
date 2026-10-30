// SPDX-License-Identifier: GPL-2.0

//! One IO's end, waited on: by a task, awaiting it - Done<waiter::Async> - or
//! a thread, sleeping - Done<waiter::Sync>.
//!
//! - Done: one word, where the IO's completer finds it - a bio's bi_private,
//!   or a field C reaches by container_of. Born with its waiter: owed.
//! - Completion: a borrow of the Done, awaited or waited on.
//!
//! The two are made as a pair, so an owed Done always has its waiter waiting
//! on it - the Completion's drop waits, too:
//!
//!     let c = Completion::new(&mut slot, waiter::Async::current().await);
//!     c.complete();               // its end, once - by the IO's completer
//!     c.resume_on(at).await;      // a thread: waiter::Sync::current(), c.wait(how)
//!
//! The word:
//!
//!     waiter    owed, the waiter waits
//!     null      done

use crate::c;
use crate::util::rcu;
use crate::util::sleep::{sleep_until, Sleep};
use crate::util::task::{current_task, Resumable, TaskRef};
use core::ffi::c_void;
use core::marker::PhantomData;
use core::pin::Pin;
use core::ptr::{null_mut, NonNull};
use core::sync::atomic::{AtomicPtr, Ordering};
use core::task::{Context, Poll};

/* Waiters: a thing to wake, and how */

/// Who waits on a Done: the current thread, sleeping, or the task polling us,
/// awaiting it - got the same way, a task's awaited.
pub mod waiter {
    use super::{c, current_task, NonNull, TaskRef};
    use core::future::Future;

    /// A thread, by its task_struct, with no ref.
    #[derive(Clone, Copy)]
    pub struct Sync(pub(super) NonNull<c::task_struct>);

    impl Sync {
        pub fn current() -> Self {
            Sync(NonNull::new(unsafe { c::bch2_current() })
                 .expect("a blocking wait needs a task - a kthread, in userspace"))
        }
    }

    /// A task of ours.
    #[derive(Clone, Copy)]
    pub struct Async(pub(super) TaskRef);

    impl Async {
        /// The task polling us: Ready at once.
        pub async fn current() -> Self {
            Async(current_task().await)
        }

        /// @task, for its owner, making the Done of an IO it submits before
        /// the task runs.
        ///
        /// # Safety
        /// @task waits on the Done, or drops its Completion: waited on
        /// elsewhere, the IO would still wake @task, which may be gone.
        pub unsafe fn of<F: Future<Output = ()> + Send>(task: &super::super::Task<'_, F>) -> Self {
            Async(task.task())
        }
    }
}

/// A waiter, as the pointer in a Done's word, and how complete() wakes it -
/// both sealed: the Done's alone, as only complete()'s RCU read lock keeps
/// the waiter there to wake.
pub trait Waiter: sealed::Waiter + Copy + 'static {
    /// Whether the thread dropping a Completion is its waiter, so complete()
    /// wakes it: a thread's is, its Completion !Send; a task's isn't.
    const WAKES_DROPPER: bool;
}

mod sealed {
    use core::ptr::NonNull;

    pub trait Waiter {
        fn as_ptr(self) -> NonNull<()>;
        /// # Safety
        /// @p is a waiter of this kind, from a Done's word; complete() holds
        /// the RCU read lock, which keeps it.
        unsafe fn wake(p: NonNull<()>);
    }
}

impl Waiter for waiter::Sync {
    const WAKES_DROPPER: bool = true;
}

impl sealed::Waiter for waiter::Sync {
    fn as_ptr(self) -> NonNull<()> {
        self.0.cast()
    }

    unsafe fn wake(p: NonNull<()>) {
        unsafe { c::wake_up_process(p.as_ptr().cast()) };
    }
}

impl Waiter for waiter::Async {
    const WAKES_DROPPER: bool = false;
}

impl sealed::Waiter for waiter::Async {
    fn as_ptr(self) -> NonNull<()> {
        (self.0).0.cast()
    }

    unsafe fn wake(p: NonNull<()>) {
        unsafe { super::wake(p.cast()) };
    }
}

/* The protocol, and the Completion */

#[repr(transparent)]
pub struct Done<W: Waiter = waiter::Async> {
    word: AtomicPtr<()>,
    _w:   PhantomData<fn() -> W>,
}

impl<W: Waiter> Done<W> {
    fn new(waiter: W) -> Self {
        Done { word: AtomicPtr::new(waiter.as_ptr().as_ptr()), _w: PhantomData }
    }

    /// # Safety
    /// As from_ptr()'s.
    unsafe fn init_at<'a>(p: *mut *mut c_void, waiter: W) -> &'a Self {
        unsafe {
            p.write(waiter.as_ptr().as_ptr().cast());
            Self::from_ptr(p)
        }
    }

    /// # Safety
    /// @p is valid for 'a, and reached only as a Done meanwhile.
    pub unsafe fn from_ptr<'a>(p: *mut *mut c_void) -> &'a Self {
        unsafe { &*p.cast::<Self>() }
    }

    pub fn complete(&self) {
        let _rcu = rcu::read_lock();
        // Relaxed: stored at the Done's birth, before the IO was handed out
        let w = NonNull::new(self.word.load(Ordering::Relaxed)).expect("Done completed twice");
        self.word.store(null_mut(), Ordering::Release);
        // seeing null, the waiter may return and be freed: RCU keeps it
        unsafe { W::wake(w) };
    }

    pub fn done(&self) -> bool {
        self.word.load(Ordering::Acquire).is_null()
    }
}

#[must_use = "an IO's Completion is waited on, or its drop waits for it"]
pub struct Completion<'a, W: Waiter = waiter::Async> {
    d:  &'a Done<W>,
    /// Send as the waiter is: a thread's isn't, as complete() wakes the
    /// thread it was born on, and its drop waits there
    _w: PhantomData<W>,
}

impl<'a, W: Waiter> Completion<'a, W> {
    /// A Done in @slot, born with @waiter, and the wait on it.
    pub fn new(slot: &'a mut Option<Done<W>>, waiter: W) -> Self {
        Completion { d: slot.insert(Done::new(waiter)), _w: PhantomData }
    }

    /// new(), the Done at @p: a bi_private.
    ///
    /// # Safety
    /// As Done::from_ptr()'s.
    pub unsafe fn init_at(p: *mut *mut c_void, waiter: W) -> Self {
        Completion { d: unsafe { Done::init_at(p, waiter) }, _w: PhantomData }
    }
}

impl<W: Waiter> Drop for Completion<'_, W> {
    fn drop(&mut self) {
        if W::WAKES_DROPPER {
            let Ok(()) = sleep_until(Sleep::IDLE, || self.d.done().then_some(()));
        } else {
            // complete() wakes the task, not the thread dropping it
            while !self.d.done() {
                let _ = sleep_until(Sleep::IDLE.timeout(1), || self.d.done().then_some(()));
            }
        }
    }
}

impl Completion<'_, waiter::Sync> {
    pub fn wait(self, how: Sleep) {
        let Ok(()) = sleep_until(how, || self.d.done().then_some(()));
    }
}

impl Resumable for Completion<'_, waiter::Async> {
    type Output = ();

    fn poll_resume(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        if self.d.done() { Poll::Ready(()) } else { Poll::Pending }
    }
}
