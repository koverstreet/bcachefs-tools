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
//! ```ignore
//! let c = Completion::new(&mut done, waiter::Async::current().await);
//! c.complete();               // its end, once - by the IO's completer
//! c.resume_on(at).await;      // a thread: waiter::Sync::current(), c.wait(how)
//! ```
//!
//! The word:
//!
//! ```text
//! waiter    owed, the waiter waits
//! null      done
//! ```
//!
//! A task's wake (TaskRef::wake(), task/mod.rs) writes to the task only if
//! it's parked on this Done.

use crate::c;
use crate::util::rcu;
use crate::util::sleep::{sleep_until, Sleep};
use crate::util::task::{current_task, task_in, Resumable, TaskRef};
use core::cell::UnsafeCell;
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
    use super::{c, current_task, task_in, Context, NonNull, TaskRef};

    /// A thread, by its task_struct, with no ref.
    #[derive(Clone, Copy)]
    pub struct Sync(pub(super) NonNull<c::task_struct>);

    impl Sync {
        pub fn current() -> Self {
            Sync(NonNull::new(crate::util::sched::current())
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

        /// The task polling @cx: current(), in a poll.
        pub fn of(cx: &mut Context<'_>) -> Self {
            Async(task_in(cx).expect("polled outside a task of ours"))
        }

        /// @task, for its owner, making the Done of an IO it submits before
        /// the task runs: a TaskSlot's, for the task to be.
        ///
        /// # Safety
        /// @task waits on the Done, or drops its Completion: waited on
        /// elsewhere, the IO would still wake @task, which may be gone. And
        /// the task's made before the Done's completed.
        pub unsafe fn of_ref(task: TaskRef) -> Self {
            Async(task)
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
        /// @p is a waiter of this kind, from @done's word, which complete()
        /// has just marked done; complete() holds the RCU read lock, which
        /// keeps it.
        unsafe fn wake(p: NonNull<()>, done: NonNull<()>);
    }
}

impl Waiter for waiter::Sync {
    const WAKES_DROPPER: bool = true;
}

impl sealed::Waiter for waiter::Sync {
    fn as_ptr(self) -> NonNull<()> {
        self.0.cast()
    }

    unsafe fn wake(p: NonNull<()>, _done: NonNull<()>) {
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

    unsafe fn wake(p: NonNull<()>, done: NonNull<()>) {
        unsafe { super::TaskRef(p.cast()).wake(done.cast()) };
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

    /// Not owed: a wait's, before it's begun or once it's done.
    pub fn completed() -> Self {
        Done { word: AtomicPtr::new(null_mut()), _w: PhantomData }
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
        let w = NonNull::new(self.word.load(Ordering::Relaxed)).expect("Done completed when not owed: twice, or never born");
        self.word.store(null_mut(), Ordering::Release);
        // seeing null, the waiter may return and be freed: RCU keeps it
        unsafe { W::wake(w, NonNull::from(self).cast()) };
    }

    pub fn done(&self) -> bool {
        self.word.load(Ordering::Acquire).is_null()
    }

    /// done(), as the load after run()'s store of the task's @waiting: SeqCst,
    /// the pair of a Dekker pair - a plain load on x86, ldar on arm64.
    pub(super) fn done_ordered(&self) -> bool {
        self.word.load(Ordering::SeqCst).is_null()
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
    /// @d, not owed, born with @waiter, and the wait on it.
    pub fn new(d: &'a mut Done<W>, waiter: W) -> Self {
        assert!(d.done(), "a Done born while it's owed");
        *d = Done::new(waiter);
        Completion { d, _w: PhantomData }
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

    fn poll_resume(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.d.done() {
            return Poll::Ready(());
        }
        // run() parks the task on it, once the poll's returned
        super::park_on(cx, self.d);
        Poll::Pending
    }
}

/* A Done that carries its completer's value */

/// A Done, and what its completer leaves for its waiter: one IO's, or a
/// task's, end with its result.
pub struct DoneWith<T, W: Waiter = waiter::Async> {
    done:  Done<W>,
    value: UnsafeCell<Option<T>>,
}

unsafe impl<T: Send, W: Waiter> Sync for DoneWith<T, W> {}

/// A Completion that returns what its completer left: None if it was dropped
/// without leaving anything.
#[must_use = "a Completion is waited on, or its drop waits for it"]
pub struct CompletionReturn<'a, T, W: Waiter = waiter::Async> {
    c: Completion<'a, W>,
    d: &'a DoneWith<T, W>,
}

/// The completer's side of a CompletionReturn: complete() leaves its value
/// and completes the Done, once - dropped, it completes it with none.
pub struct Return<T, W: Waiter = waiter::Async>(NonNull<DoneWith<T, W>>);

unsafe impl<T: Send, W: Waiter> Send for Return<T, W> {}

impl<'a, T, W: Waiter> CompletionReturn<'a, T, W> {
    /// A DoneWith in @slot, born with @waiter: the wait on it, and its
    /// completer. The completer holds no borrow: the wait's drop waits for it.
    pub fn new(slot: &'a mut Option<DoneWith<T, W>>, waiter: W) -> (Self, Return<T, W>) {
        let d = slot.insert(DoneWith { done: Done::new(waiter), value: UnsafeCell::new(None) });
        let ret = Return(NonNull::from(&*d));
        (CompletionReturn { c: Completion { d: &d.done, _w: PhantomData }, d }, ret)
    }

    fn take(&self) -> Option<T> {
        unsafe { (*self.d.value.get()).take() }
    }
}

impl<T> CompletionReturn<'_, T, waiter::Sync> {
    pub fn wait(self, how: Sleep) -> Option<T> {
        let Ok(()) = sleep_until(how, || self.d.done.done().then_some(()));
        self.take()
    }
}

impl<T> Resumable for CompletionReturn<'_, T, waiter::Async> {
    type Output = Option<T>;

    fn poll_resume(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        let this = self.get_mut();
        match Pin::new(&mut this.c).poll_resume(cx) {
            Poll::Ready(()) => Poll::Ready(this.take()),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<T, W: Waiter> Return<T, W> {
    pub fn complete(self, v: T) {
        let d = core::mem::ManuallyDrop::new(self).0;
        unsafe {
            *(*d.as_ptr()).value.get() = Some(v);
            (*d.as_ptr()).done.complete();
        }
    }
}

impl<T, W: Waiter> Drop for Return<T, W> {
    fn drop(&mut self) {
        unsafe { (*self.0.as_ptr()).done.complete() };
    }
}
