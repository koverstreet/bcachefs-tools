// SPDX-License-Identifier: GPL-2.0

//! One IO's end, waited on: by a task, awaiting it - Done<waiter::Async> - or
//! a thread, sleeping - Done<waiter::Sync>.
//!
//! - Done: one word, where the IO's completer finds it - a bio's bi_private,
//!   or a field C reaches by container_of. Born with its waiter: owed.
//! - Completion: a borrow of the Done, awaited or waited on - made with it,
//!   so an owed Done always has its waiter waiting on it: the Completion's
//!   drop waits, too.
//! - DynDone: a Done's completer, of either waiter's, for a waitlist: one per
//!   owed Done - complete() consumes it, and dropping it completes it.
//!
//! ```ignore
//! let (c, d) = Completion::new_dyn(&mut done, waiter::Async::current().await);
//! d.complete();               // its end, once - by the IO's completer: consumed
//! c.resume_on(at).await;      // a thread: waiter::Sync::current(), c.wait(how)
//! ```
//!
//! An Armable is a Done that may never be owed - a journal flush's wait, if
//! nothing attaches it: made with the one Unarmed that may arm it, which
//! arming consumes for the DynDone.
//!
//! A DoneWith is a Done and what its completer leaves - a task's result: its
//! CompletionReturn returns it, and its completer, a Return, leaves it as it
//! completes.
//!
//! The word:
//!
//! ```text
//! waiter    owed, the waiter waits
//! null      not owed: done, or an Armable's not armed
//! ```
//!
//! A task's wake (TaskRef::wake(), task/mod.rs) writes to the task only if
//! it's parked on this Done.

use crate::c;
use crate::util::rcu;
use crate::util::sleep::{sleep_until, Sleep};
use crate::util::task::{current_task, task_in, Resumable, TaskRef};
use core::cell::{Cell, UnsafeCell};
use core::ffi::c_void;
use core::marker::{PhantomData, PhantomPinned};
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
            Sync(NonNull::new(crate::os::sched::current())
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

    /// # Safety
    /// It's owed, and this is its one completion.
    pub unsafe fn complete(&self) {
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

    /// Waited out, as its wait goes: its waiter's thread, or not.
    fn wait_out(&self) {
        if W::WAKES_DROPPER {
            let Ok(()) = sleep_until(Sleep::IDLE, || self.done().then_some(()));
        } else {
            // complete() wakes the task, not the thread dropping its wait
            while !self.done() {
                let _ = sleep_until(Sleep::IDLE.timeout(1), || self.done().then_some(()));
            }
        }
    }

    fn sleep_until_done(&self, how: Sleep) {
        let Ok(()) = sleep_until(how, || self.done().then_some(()));
    }
}

impl Done<waiter::Async> {
    fn poll_done(&self, cx: &mut Context<'_>) -> Poll<()> {
        if self.done() {
            return Poll::Ready(());
        }
        // run() parks the task on it, once the poll's returned
        super::park_on(cx, self);
        Poll::Pending
    }
}

/// A Done's completer, of either waiter's, for a list that completes what's on
/// it without knowing whose: a futex bucket's, a journal flush's. 'a is
/// what it completes - a Select's input borrows the Select; a Done whose
/// waiter waits for it, as a Completion's drop does, is 'static.
pub struct DynDone<'a> {
    p:   NonNull<()>,
    end: unsafe fn(NonNull<()>),
    _l:  PhantomData<&'a ()>,
}

// a Done's completer may be anywhere: it's completed as a &Done is
unsafe impl Send for DynDone<'_> {}
unsafe impl Sync for DynDone<'_> {}

impl<'a> DynDone<'a> {
    /// # Safety
    /// @d is owed, and this is its one completer.
    unsafe fn of<W: Waiter>(d: &Done<W>) -> Self {
        unsafe fn end<W: Waiter>(d: NonNull<()>) {
            unsafe { d.cast::<Done<W>>().as_ref().complete() }
        }
        DynDone { p: NonNull::from(d).cast(), end: end::<W>, _l: PhantomData }
    }

    /// # Safety
    /// @end(@p) is sound once, when this is completed or dropped, within 'a.
    pub unsafe fn from_fn(p: NonNull<()>, end: unsafe fn(NonNull<()>)) -> Self {
        DynDone { p, end, _l: PhantomData }
    }

    /// Its drop's the completion.
    pub fn complete(self) {}
}

impl Drop for DynDone<'_> {
    fn drop(&mut self) {
        unsafe { (self.end)(self.p) }
    }
}

/* A Done that may never be owed */

/// A completion that's only owed if what it waits on asks for it - a journal
/// flush's, if a commit attaches it: a Done not owed yet and its waiter, made
/// in place with the one Unarmed that may arm it. Waited on, it's done at
/// once if it never was armed; dropped, it waits for its Done if it was.
/// Pinned: once armed, its completer points into it.
pub struct Armable<W: Waiter = waiter::Async> {
    done:   Done<W>,
    waiter: W,
    /* set by its arming, by its waiter's task: what its Done can't say
     * once it's completed */
    armed:  Cell<bool>,
    _pin:   PhantomPinned,
}

/// The right to arm an Armable, once. Dropped, the Armable never is.
pub struct Unarmed<'a> {
    a:     NonNull<()>,
    arm:   unsafe fn(NonNull<()>) -> DynDone<'static>,
    _slot: PhantomData<&'a mut ()>,
}

// as a DynDone: armed and completed as a &Done is
unsafe impl Send for Unarmed<'_> {}

impl<W: Waiter> Armable<W> {
    /// One, in @slot, for @waiter - what was there dropped, as it waits for
    /// its Done - and the right to arm it.
    pub fn new(mut slot: Pin<&mut Option<Self>>, waiter: W) -> Unarmed<'_> {
        unsafe fn arm<W: Waiter>(p: NonNull<()>) -> DynDone<'static> {
            let a = unsafe { p.cast::<Armable<W>>().as_ref() };
            a.done.word.store(a.waiter.as_ptr().as_ptr(), Ordering::Relaxed);
            a.armed.set(true);
            unsafe { DynDone::of(&a.done) }
        }
        slot.set(Some(Armable { done: Done::completed(), waiter, armed: Cell::new(false), _pin: PhantomPinned }));
        let a = slot.into_ref().get_ref().as_ref().expect("just set");
        Unarmed { a: NonNull::from(a).cast(), arm: arm::<W>, _slot: PhantomData }
    }

    /// The wait on it: done at once, if it was never armed.
    pub fn completion(self: Pin<&mut Self>) -> Completion<'_, W> {
        Completion { d: &self.into_ref().get_ref().done, _w: PhantomData }
    }

    pub fn armed(&self) -> bool {
        self.armed.get()
    }
}

impl<W: Waiter> Drop for Armable<W> {
    fn drop(&mut self) {
        self.done.wait_out();
    }
}

impl Unarmed<'_> {
    pub fn arm(self) -> DynDone<'static> {
        unsafe { (self.arm)(self.a) }
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

    pub fn new_dyn(d: &'a mut Done<W>, waiter: W) -> (Self, DynDone<'static>) {
        let c = Self::new(d, waiter);
        let d = unsafe { DynDone::of(c.d) };
        (c, d)
    }

    pub fn is_done(&self) -> bool {
        self.d.done()
    }
}

impl<W: Waiter> Drop for Completion<'_, W> {
    fn drop(&mut self) {
        self.d.wait_out();
    }
}

impl Completion<'_, waiter::Sync> {
    pub fn wait(self, how: Sleep) {
        self.d.sleep_until_done(how);
    }
}

impl Resumable for Completion<'_, waiter::Async> {
    type Output = ();

    fn poll_resume(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        self.d.poll_done(cx)
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

impl<T, W: Waiter> DoneWith<T, W> {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        DoneWith { done: Done::completed(), value: UnsafeCell::new(None) }
    }
}

/// A Completion that returns what its completer left: None if it was dropped
/// without leaving anything.
#[must_use = "a Completion is waited on, or its drop waits for it"]
pub struct CompletionReturn<'a, T, W: Waiter = waiter::Async> {
    d:  &'a DoneWith<T, W>,
    /// Send as the waiter is, as a Completion's
    _w: PhantomData<W>,
}

/// The completer's side of a CompletionReturn: complete() leaves its value
/// and completes the Done, once - dropped, it completes it with none.
pub struct Return<T, W: Waiter = waiter::Async>(NonNull<DoneWith<T, W>>);

unsafe impl<T: Send, W: Waiter> Send for Return<T, W> {}

impl<'a, T, W: Waiter> CompletionReturn<'a, T, W> {
    /// @d, not owed, born with @waiter: the wait on it, and its completer.
    /// The completer holds no borrow: the wait's drop waits for it.
    pub fn new(d: &'a mut DoneWith<T, W>, waiter: W) -> (Self, Return<T, W>) {
        assert!(d.done.done(), "a DoneWith born while it's owed");
        d.done = Done::new(waiter);
        *d.value.get_mut() = None;
        let ret = Return(NonNull::from(&*d));
        (CompletionReturn { d, _w: PhantomData }, ret)
    }

    fn take(&self) -> Option<T> {
        unsafe { (*self.d.value.get()).take() }
    }
}

impl<T, W: Waiter> Drop for CompletionReturn<'_, T, W> {
    fn drop(&mut self) {
        self.d.done.wait_out();
    }
}

impl<T> CompletionReturn<'_, T, waiter::Sync> {
    pub fn wait(self, how: Sleep) -> Option<T> {
        self.d.done.sleep_until_done(how);
        self.take()
    }
}

impl<T> Resumable for CompletionReturn<'_, T, waiter::Async> {
    type Output = Option<T>;

    fn poll_resume(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        let this = self.get_mut();
        this.d.done.poll_done(cx).map(|()| this.take())
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
