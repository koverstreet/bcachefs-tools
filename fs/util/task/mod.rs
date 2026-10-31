// SPDX-License-Identifier: GPL-2.0

//! An async executor: a Task is a future and what runs it, in memory its
//! owner owns - a field of what it's the task of, a bio's front pad, a box.
//! The executor never allocates or frees one. A task runs on a workqueue, a
//! RunQueue, or a kthread of its own, created with it; it runs or, never
//! started, is dropped as any value is.
//!
//! A task's last act is its done function, which the executor calls once the
//! future has returned - after its last touch of the task. That's where the
//! owner learns it's done, and may free it: a joiner woken, a bio ended, a box
//! dropped. Dropped any other time it's running, a Task panics.
//!
//! A task's future is made in place, never moved, so a big one is never on
//! the spawner's stack. An awaited future is the opposite: rustc builds it on
//! the poller's stack and moves it into the awaiter, so what's big belongs in
//! the task's own future's locals (stack_pin_init!()).
//!
//! Every await says where the task resumes - resume_on(), which makes our
//! awaitables (Resumable) Futures:
//! - Queue is recorded in the task's header while the await is pending; a
//!   wake dispatches there. Spawned leaves it: where the task was made to run,
//!   unless an await around this one says otherwise. A RunQueue's task, or a
//!   kthread's, runs only there.
//! - Inline resumes it in the waker's context, an endio's: everything up to
//!   the next await runs there, and must not sleep or take a lock that isn't
//!   irqsave - C's continue_at(cl, fn, NULL), unchecked.
//!
//! A task is running or waiting, and @waiting says which: the Done it's
//! parked on, or null - running, dispatched, or not started or done. It's the
//! task's lock: whoever takes it from a Done to null owns the task and runs
//! it, so it's never polled twice at once. An await that's pending names its
//! Done in the poll's Polled - one at a time - and run() publishes it once the
//! poll has returned, every frame having stored where it resumes; the
//! completer, having marked its Done, takes it back if it's still the one
//! named. Between them it's the wait_event() protocol: each side stores, then
//! reads the other's store, ordered. A completion of a Done the task isn't
//! parked on writes nothing to the task: running, it sees the Done when it
//! next looks.
//!
//! A poll finds its task in its Context's ext(), which run() fills in -
//! current_task(). Nothing wakes through a Waker: what a task awaits holds
//! the task, a Done its waiter, and wakes that. A Context needs one, so it's
//! Waker::noop().
//!
//! A kthread task's thread is woken when the task's dispatched to it, and
//! sleeps till it's claimed - @waiting null - which, as it runs nowhere else,
//! means dispatched there. Its done runs there, so the thread's last touch is
//! the task's, and it exits on its own; the task's drop stops it only if it
//! never started.
//!
//! The rest: completion.rs, an IO's end, that a task awaits or a thread waits
//! on; queue.rs, the
//! RunQueue; box.rs, a task in a box of its own (TaskBox) or in one with what
//! it's the task of (BoxWithTask).

mod r#box;
pub mod completion;
mod queue;

pub use completion::{waiter, Completion, Done, Waiter};
pub use queue::RunQueue;
pub use r#box::{spawn_detached, BoxWithTask, TaskBox};

use crate::c;
use crate::errcode::BchError;
use crate::util::kthread::{Kthread, KthreadRef};
use crate::util::rcu::{self, RcuMember};
use crate::util::sleep::{sleep_until, Sleep};
use crate::util::workqueue::{Queue, RawWork};
use core::fmt;
use core::future::Future;
use core::cell::Cell;
use core::marker::{PhantomData, PhantomPinned};
use core::mem::MaybeUninit;
use core::pin::Pin;
use core::ptr::{null_mut, NonNull};
use core::sync::atomic::{AtomicPtr, Ordering};
use core::task::{Context, ContextBuilder, Poll, Waker};
use pin_init::{pin_data, pin_init, pinned_drop, PinInit};

#[derive(Clone, Copy)]
pub enum Resume<'a> {
    Inline,
    Queue(&'a Queue),
    Spawned,
}

pub trait Resumable {
    type Output;

    fn poll_resume(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output>;

    fn resume_on(self, r: Resume<'_>) -> Resumed<'_, Self>
    where
        Self: Sized,
    {
        Resumed { inner: self, target: r, recorded: None, _pin: PhantomPinned }
    }
}

impl<F: Future> Resumable for F {
    type Output = F::Output;

    fn poll_resume(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        Future::poll(self, cx)
    }
}

/// An await, recording where its task resumes while it's pending. Pinned
/// once polled, and checked to be polled by the one task: the drop of one
/// that's pending writes its task's header, which must be the task it's in.
#[must_use = "a future does nothing unless awaited"]
#[pin_data(PinnedDrop)]
pub struct Resumed<'r, A> {
    #[pin]
    inner:    A,
    target:   Resume<'r>,
    /// While it's pending: what we set @next to, and what it was
    recorded: Option<(TaskRef, Target, Target)>,
    #[pin]
    _pin:     PhantomPinned,
}

impl<A: Resumable> Future for Resumed<'_, A> {
    type Output = A::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<A::Output> {
        let this = self.project();

        // not our executor: nowhere to record it
        let Some(task) = task_in(cx) else {
            return this.inner.poll_resume(cx);
        };

        // SAFETY: it's polling us
        let hr = unsafe { task.0.as_ref() };
        let at = match *this.target {
            Resume::Inline => Target::Inline,
            Resume::Queue(q) => Target::Queue(NonNull::from(q)),
            Resume::Spawned => return this.inner.poll_resume(cx),
        };
        match *this.recorded {
            None => *this.recorded = Some((task, at, hr.next.get())),
            Some((t, ..)) => assert!(t == task, "an await moved from one task to another"),
        }
        hr.next.set(at);
        let ret = this.inner.poll_resume(cx);
        if ret.is_ready() {
            // done: restored now, so the drop's left nothing to touch
            if let Some((_, at, prev)) = this.recorded.take() {
                if hr.next.get() == at {
                    hr.next.set(prev);
                }
            }
        }
        ret
    }
}

#[pinned_drop]
impl<A> PinnedDrop for Resumed<'_, A> {
    fn drop(self: Pin<&mut Self>) {
        // dropped pending: our queue is only borrowed, so a wake mustn't find
        // it after us, unless an await since recorded it again
        if let Some((task, at, prev)) = self.recorded {
            // SAFETY: in its future: its poll, or its drop
            let next = &unsafe { task.0.as_ref() }.next;
            if next.get() == at {
                next.set(prev);
            }
        }
    }
}

/// Where a task runs: where it was spawned, until an await says otherwise.
/// Inline is in the waker's context. A Queue an await names is borrowed till
/// the await's done.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Target {
    Inline,
    Queue(NonNull<Queue>),
}

// SAFETY: queues are used from any context
unsafe impl Send for Target {}

/// Where a task runs: its first poll, and what Resume::Spawned resumes it on.
/// The task borrows its queue, for 'q; a kthread's name is only for its
/// creation, 'n.
#[derive(Clone, Copy)]
pub enum Spawn<'q, 'n> {
    Queue(&'q Queue),
    Run(Pin<&'q RunQueue>),
    /// A kthread of its own, named
    Thread(fmt::Arguments<'n>),
}

#[repr(C)]
#[pin_data]
struct Header {
    /// queue_work()'s. It and the four after it are what a completer's claim
    /// and dispatch() touch, kept together
    #[pin]
    work:      c::work_struct,
    /// The Done it's parked on, null running: see the top of the file
    waiting:   AtomicPtr<Done>,
    /// Written only in its polls, read by whoever's claimed it - which is what
    /// makes the Task Sync
    next:      Cell<Target>,
    /// Set, it's where the task runs, and @next is unread. It outlives the
    /// task
    runq:      Option<NonNull<RunQueue>>,
    /// As @runq: its own kthread, woken to run it
    thread:    Option<KthreadRef>,
    /// The owner's, by start()
    started:   Cell<bool>,
    /// Returned, for its drop's check
    complete:  Cell<bool>,
    /// RunQueue's singly linked list
    rq_next:   AtomicPtr<Header>,
    /// Ready: the future's dropped. Here, not behind another pointer: one load
    /// and one indirect call back into the future, from every path that runs it.
    poll:      unsafe fn(NonNull<Header>, &mut Context<'_>) -> Poll<()>,
    /// The task's last act: Task<F>'s, which calls its owner's done with it
    done:      unsafe fn(NonNull<Header>),
    /// A Done's complete() may wake it after its waiter's seen it done
    _rcu:      RcuMember,
}

/// The header first: a task is found from it.
#[repr(C)]
#[pin_data(PinnedDrop)]
pub struct Task<'q, F: Future<Output = ()>> {
    #[pin]
    header: Header,
    /// Made in place; dropped in place when it returns, or with an unstarted
    /// task.
    #[pin]
    future: MaybeUninit<F>,
    /// Its owner's, called once it's returned: see the top of the file
    done:   unsafe fn(NonNull<Self>),
    /// The queue it runs on, which the header points to
    _q:     PhantomData<&'q Queue>,
}

// SAFETY: the header's atomics, or written by whoever's claimed the task -
// @started by its owner - and the future's Send, polled anywhere
unsafe impl<F: Future<Output = ()> + Send> Send for Task<'_, F> {}
unsafe impl<F: Future<Output = ()> + Send> Sync for Task<'_, F> {}

impl<'q, F: Future<Output = ()> + Send> Task<'q, F> {
    /// To run @at, once start()ed. @done is the task's last act: see the top of
    /// the file.
    pub fn new<'n>(at: Spawn<'q, 'n>, future: impl PinInit<F> + 'n, done: unsafe fn(NonNull<Self>))
        -> impl PinInit<Self, BchError> + 'n
    where
        F: 'n,
        'q: 'n,
    {
        let (next, runq) = match at {
            Spawn::Queue(wq) => (Target::Queue(NonNull::from(wq)), None),
            // @next unread
            Spawn::Run(rq) => (Target::Inline, Some(NonNull::from(rq.get_ref()))),
            Spawn::Thread(_) => (Target::Inline, None),
        };
        // SAFETY: a MaybeUninit<F> is laid out as F
        let future = unsafe {
            pin_init::pin_init_from_closure(move |slot: *mut MaybeUninit<F>| {
                crate::util::alloc::init_at(future, slot.cast::<F>())
            })
        };
        pin_init!(Self {
            header <- pin_init!(Header {
                work <- crate::new_work!(crate::util::workqueue::run_raw::<Header>, "bcachefs task"),
                waiting:   AtomicPtr::new(null_mut()),
                next:      Cell::new(next),
                runq,
                thread:    None,
                started:   Cell::new(false),
                complete:  Cell::new(false),
                rq_next:   AtomicPtr::new(null_mut()),
                poll:      Self::poll,
                done:      Self::call_done,
                _rcu:      RcuMember,
            }),
            future <- future,
            done,
            _q:     PhantomData,
        }? BchError)
        // its thread, once it's in place; on error it's dropped, unstarted
        .pin_chain(move |task| {
            if let Spawn::Thread(name) = at {
                let t = task.task();
                // SAFETY: the thread's task
                let run = move || unsafe { run_thread(t) };
                // SAFETY: the task outlives its thread: see Task's drop
                let thread = unsafe { KthreadRef::create(name, run) }?;
                *task.project().header.project().thread = Some(thread);
            }
            Ok(())
        })
    }

    /// Its future, to finish setting up before it's started.
    pub fn future(self: Pin<&mut Self>) -> Pin<&mut F> {
        assert!(!self.header.started.get(), "a task's future, after it's started");
        // SAFETY: made in place, pinned with the task; not started, so ours
        unsafe { Pin::new_unchecked(self.get_unchecked_mut().future.assume_init_mut()) }
    }

    pub fn task(&self) -> TaskRef {
        TaskRef(NonNull::from(&self.header))
    }

    pub fn start(self: Pin<&Self>) {
        // SAFETY: pinned, so it stays put; its drop panics while it runs
        unsafe { self.task().start() };
    }

    /// # Safety
    /// @h is a Task<F>'s, claimed by us.
    unsafe fn poll(h: NonNull<Header>, cx: &mut Context<'_>) -> Poll<()> {
        let task = h.cast::<Self>().as_ptr();
        let future = unsafe { (&raw mut (*task).future).cast::<F>() };
        // SAFETY: made in the task, which never moves
        let ret = unsafe { Pin::new_unchecked(&mut *future) }.poll(cx);
        if ret.is_ready() {
            // SAFETY: returned: never polled again
            unsafe { core::ptr::drop_in_place(future) };
        }
        ret
    }

    /// # Safety
    /// @h is a Task<F>'s, returned: its last act.
    unsafe fn call_done(h: NonNull<Header>) {
        let task = h.cast::<Self>();
        unsafe { ((*task.as_ptr()).done)(task) };
    }
}

#[pinned_drop]
impl<F: Future<Output = ()>> PinnedDrop for Task<'_, F> {
    fn drop(self: Pin<&mut Self>) {
        // SAFETY: not moved out of
        let this = unsafe { self.get_unchecked_mut() };
        let started = this.header.started.get();
        assert!(!started || this.header.complete.get(), "a task freed while it runs");
        if !started {
            // SAFETY: made, never polled
            unsafe { core::ptr::drop_in_place(this.future.as_mut_ptr()) };
        }
        if let Some(thread) = this.header.thread.take() {
            // never started, it's stopped before it ran; done, it ran the
            // done that freed us, and exits on its own
            if !started {
                thread.stop();
            }
        }
    }
}

/* Running a task: */

/// # Safety
/// @h is a live task's, claimed by the caller, not running.
unsafe fn dispatch(h: NonNull<Header>) {
    let hr = unsafe { h.as_ref() };
    if let Some(thread) = &hr.thread {
        thread.wake();
        return;
    }
    if let Some(rq) = hr.runq {
        // SAFETY: claimed and not running, so not on its list, or queued, already
        unsafe { rq.as_ref().push(h) };
        return;
    }
    // SAFETY: a target is live while it's recorded
    match hr.next.get() {
        Target::Inline => {
            unsafe { run(h) };
        }
        Target::Queue(q) => {
            unsafe { q.as_ref().queue_work(&raw mut (*h.as_ptr()).work) };
        }
    }
}

impl RawWork for Header {
    const WORK: usize = core::mem::offset_of!(Header, work);

    #[inline(always)]
    unsafe fn run(h: NonNull<Self>) {
        unsafe { self::run(h) };
    }
}

/// A task's kthread: runs it each time it's claimed - dispatched here, as it
/// runs nowhere else - until it's done; then exits. It sleeps freezable,
/// which freezes it only once the task's made it freezable
/// (Kthread::set_freezable()).
///
/// # Safety
/// @t is the thread's task, which outlives it.
unsafe fn run_thread(t: TaskRef) {
    // stopped before it was started: userspace still runs us
    if Kthread::should_stop() {
        return;
    }
    let h = t.0;
    loop {
        // SAFETY: the task outlives us, till its done
        let hr = unsafe { h.as_ref() };
        let Ok(()) = sleep_until(Sleep::IDLE.freezable(),
            || hr.waiting.load(Ordering::Acquire).is_null().then_some(()));
        // SAFETY: dispatched here: ours to run. Done, it may be gone
        if unsafe { run(h) } {
            return;
        }
    }
}

/// Poll the task, until it's done or parked - or wants to run elsewhere: true
/// if it's done, when it may be gone.
///
/// # Safety
/// @h is a live task's, dispatched to us: claimed, ours.
unsafe fn run(h: NonNull<Header>) -> bool {
    let hr = unsafe { h.as_ref() };
    debug_assert!(hr.waiting.load(Ordering::Relaxed).is_null() && !hr.complete.get());
    // where we are: its target only changes in a poll, and none's run since
    // it was dispatched here; a RunQueue's or a thread's task runs only there
    let here = hr.next.get();
    let fixed = hr.runq.is_some() || hr.thread.is_some();
    let mut polled = Polled { task: TaskRef(h), parks_on: None };
    let mut cx = ContextBuilder::from_waker(Waker::noop()).ext(&mut polled).build();

    loop {
        // SAFETY: claimed, by us
        if unsafe { (hr.poll)(h, &mut cx) }.is_ready() {
            let done = hr.done;
            hr.complete.set(true);
            // the last act: the owner may free it
            unsafe { done(h) };
            return true;
        }

        let d = polled_of(&mut cx).parks_on.take()
            .expect("a task pending on nothing: nothing will run it again");
        // SAFETY: still ours, so its future - the Done in it - is where it was
        if !unsafe { d.as_ref() }.done() {
            // the unlock, every frame of the poll having stored where it
            // resumes: from here a completer may claim it, and run it, and the
            // task and the Done be gone but for RCU
            let _rcu = rcu::read_lock();
            // the store, ordered - an xchg, as smp_store_mb() - then the load:
            // complete()'s store and claim()'s compare_exchange the other side
            hr.waiting.store(d.as_ptr(), Ordering::SeqCst);
            if !unsafe { d.as_ref() }.done_ordered() {
                return false;
            }
            // done meanwhile: ours again, unless its completer has it
            if hr.waiting.compare_exchange(d.as_ptr(), null_mut(), Ordering::SeqCst, Ordering::Relaxed).is_err() {
                return false;
            }
        }

        // ours: repolled here, or run where it's now to resume
        if !fixed && hr.next.get() != here {
            unsafe { dispatch(h) };
            return false;
        }
    }
}

/// A Done's completer, having marked it done: the task's, if it's parked on
/// it - and dispatched. If it isn't, nothing's written: it's running, and sees
/// the Done when it next looks, or it's parked on another.
///
/// # Safety
/// @h is a task RCU keeps - the caller's in a read lock - and @d is the Done
/// just marked.
unsafe fn claim(h: NonNull<Header>, d: NonNull<Done>) {
    let hr = unsafe { h.as_ref() };
    // One locked op: the read of @waiting, the claim, and the barrier between
    // it and the store marking @d done, which run()'s store and load pair
    // with. That's the kernel memory model's: a value-returning cmpxchg is
    // fully ordered - lock cmpxchg, casal - so the store can't pass it. (The
    // Rust model promises less for a Release store before an RMW; there's no
    // smp_mb__before_atomic() to say it.)
    if hr.waiting.compare_exchange(d.as_ptr(), null_mut(), Ordering::SeqCst, Ordering::Relaxed).is_ok() {
        // SAFETY: claimed: ours, and alive till it's run
        unsafe { dispatch(h) };
    }
}

/// A task by its header, not knowing its future's type: to start it, and as a
/// Done's waiter. No ref: what holds one knows the task's alive.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct TaskRef(NonNull<Header>);

// SAFETY: a pointer; what's reached through it is @waiting's, the task's lock
unsafe impl Send for TaskRef {}
unsafe impl Sync for TaskRef {}

/// The task polling us: Ready at once.
pub fn current_task() -> impl Future<Output = TaskRef> {
    core::future::poll_fn(|cx| Poll::Ready(task_in(cx).expect("polled outside a task of ours")))
}

/// What run() lends each poll, in its Context's ext().
struct Polled {
    /// The task being polled
    task:     TaskRef,
    /// The Done the pending await waits on, for run() to publish
    parks_on: Option<NonNull<Done>>,
}

/// The task polling us, if it's one of ours.
fn task_in(cx: &mut Context<'_>) -> Option<TaskRef> {
    cx.ext().downcast_mut::<Polled>().map(|p| p.task)
}

fn polled_of<'a>(cx: &'a mut Context<'_>) -> &'a mut Polled {
    cx.ext().downcast_mut::<Polled>().expect("polled outside a task of ours")
}

/// An await of @d, pending: named, for run() to publish once the poll's
/// returned. An await waits on one thing at a time.
fn park_on(cx: &mut Context<'_>, d: &Done) {
    let prev = polled_of(cx).parks_on.replace(NonNull::from(d));
    assert!(prev.is_none(), "two awaits pending in one poll: a task waits on one thing at a time");
}

impl TaskRef {
    /// Dispatched, for its first poll: once.
    ///
    /// # Safety
    /// The task stays where it is, alive, till its done.
    pub unsafe fn start(self) {
        // its owner's till now: ours to dispatch
        assert!(!unsafe { self.0.as_ref() }.started.replace(true), "a task started twice");
        unsafe { dispatch(self.0) };
    }
}
