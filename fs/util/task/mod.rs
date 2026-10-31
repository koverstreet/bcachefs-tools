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
//! The state word keeps a task from being polled twice at once: a wake may
//! dispatch it to another target while it's still running. OWNED is a task
//! someone may run - dispatched, running, or not yet started or done, which
//! nobody may; a wake of an owned task sets NOTIFIED instead, and the poll
//! that's to come, or that's running, repolls. COMPLETE is a task that's
//! returned, for its drop's check.
//!
//! A poll finds its task in its Context's ext(), which run() fills in -
//! current_task(). Nothing wakes through a Waker: what a task awaits holds
//! the task, a Done its waiter, and wakes that. A Context needs one, so it's
//! Waker::noop().
//!
//! A kthread task's thread is woken when the task's dispatched to it, and
//! sleeps till it's owned - which, as it runs nowhere else, means dispatched
//! there. Its done runs there, so the thread's last touch is the task's, and
//! it exits on its own; the task's drop stops it only if it never started.
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
use crate::util::rcu::RcuMember;
use crate::util::sleep::{sleep_until, Sleep};
use crate::util::workqueue::{Queue, RawWork};
use core::fmt;
use core::future::Future;
use core::cell::Cell;
use core::marker::{PhantomData, PhantomPinned};
use core::mem::MaybeUninit;
use core::pin::Pin;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
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

/* The state word: */
const OWNED:    usize = 1 << 0;
/// Woken while owned.
const NOTIFIED: usize = 1 << 1;
const COMPLETE: usize = 1 << 2;

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
    #[pin]
    work:      c::work_struct,
    state:     AtomicUsize,
    /// Written only in its polls, read by whoever the state word has given
    /// the task to - which is what makes the Task Sync
    next:      Cell<Target>,
    /// The owner's, by start()
    started:   Cell<bool>,
    /// Set, it's where the task runs, and @next is unread. It outlives the
    /// task
    runq:      Option<NonNull<RunQueue>>,
    /// As @runq: its own kthread, woken to run it
    thread:    Option<KthreadRef>,
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

// SAFETY: the header's atomics, or written by whoever the state word gives the
// task to - @started by its owner - and the future's Send, polled anywhere
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
                state:     AtomicUsize::new(OWNED),
                next:      Cell::new(next),
                started:   Cell::new(false),
                runq,
                thread:    None,
                rq_next:   AtomicPtr::new(core::ptr::null_mut()),
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
    /// @h is a Task<F>'s, OWNED by us.
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
        assert!(!started || this.header.state.load(Ordering::Acquire) & COMPLETE != 0,
                "a task freed while it runs");
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
/// @h is a live task's, OWNED by the caller, not running.
unsafe fn dispatch(h: NonNull<Header>) {
    let hr = unsafe { h.as_ref() };
    if let Some(thread) = &hr.thread {
        thread.wake();
        return;
    }
    if let Some(rq) = hr.runq {
        // SAFETY: owned and not running, so not on its list, or queued, already
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

/// A task's kthread: runs it each time it's owned - dispatched here, as it
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
            || (hr.state.load(Ordering::Acquire) & OWNED != 0).then_some(()));
        // SAFETY: dispatched here: ours to run. Done, it may be gone
        if unsafe { run(h) } {
            return;
        }
    }
}

/// Poll the task, until it's done or idle - or wants to run elsewhere: true
/// if it's done, when it may be gone.
///
/// # Safety
/// @h is a live task's, dispatched to us: OWNED, ours.
unsafe fn run(h: NonNull<Header>) -> bool {
    let hr = unsafe { h.as_ref() };
    debug_assert!(hr.state.load(Ordering::Relaxed) & (OWNED|COMPLETE) == OWNED);
    // where we are: its target only changes in a poll, and none's run since
    // it was dispatched here; a RunQueue's or a thread's task runs only there
    let here = hr.next.get();
    let fixed = hr.runq.is_some() || hr.thread.is_some();
    let mut polled = Polled(TaskRef(h));
    let mut cx = ContextBuilder::from_waker(Waker::noop()).ext(&mut polled).build();

    loop {
        // SAFETY: OWNED, by us
        if unsafe { (hr.poll)(h, &mut cx) }.is_ready() {
            let done = hr.done;
            // owned for good: a wake after does nothing
            hr.state.store(OWNED|COMPLETE, Ordering::Release);
            // the last act: the owner may free it
            unsafe { done(h) };
            return true;
        }

        // Pending: idle, unless woken meanwhile
        let mut s = hr.state.load(Ordering::Acquire);
        let repoll = loop {
            let (new, repoll) = if s & NOTIFIED != 0 {
                // repolled here, or run where it's now to resume
                (s & !NOTIFIED, fixed || hr.next.get() == here)
            } else {
                (s & !OWNED, false)
            };
            match hr.state.compare_exchange_weak(s, new, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) if repoll => break true,
                Ok(_) => {
                    if new & OWNED != 0 {
                        // SAFETY: still ours, handed on
                        unsafe { dispatch(h) };
                    }
                    break false;
                }
                Err(cur) => s = cur,
            }
        };
        if !repoll {
            return false;
        }
    }
}

/// # Safety
/// @h is a live task's.
unsafe fn wake(h: NonNull<Header>) {
    let hr = unsafe { h.as_ref() };

    let mut s = hr.state.load(Ordering::Acquire);
    loop {
        if s & NOTIFIED != 0 {
            return;
        }
        let new = s | if s & OWNED != 0 { NOTIFIED } else { OWNED };
        match hr.state.compare_exchange_weak(s, new, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => {
                if s & OWNED == 0 {
                    // SAFETY: owned by us: it can't complete, and be freed,
                    // till we've dispatched it
                    unsafe { dispatch(h) };
                }
                return;
            }
            Err(cur) => s = cur,
        }
    }
}

/// A task by its header, not knowing its future's type: to start it, and as a
/// Done's waiter. No ref: what holds one knows the task's alive.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct TaskRef(NonNull<Header>);

// SAFETY: a pointer; what's reached through it is the state word's
unsafe impl Send for TaskRef {}
unsafe impl Sync for TaskRef {}

/// The task polling us: Ready at once.
pub fn current_task() -> impl Future<Output = TaskRef> {
    core::future::poll_fn(|cx| Poll::Ready(task_in(cx).expect("polled outside a task of ours")))
}

/// What run() lends each poll, in its Context's ext(): the task being polled.
struct Polled(TaskRef);

/// The task polling us, if it's one of ours.
fn task_in(cx: &mut Context<'_>) -> Option<TaskRef> {
    cx.ext().downcast_mut::<Polled>().map(|p| p.0)
}

impl TaskRef {
    /// Dispatched, for its first poll: once.
    ///
    /// # Safety
    /// The task stays where it is, alive, till its done.
    pub unsafe fn start(self) {
        // made OWNED: ours to dispatch
        assert!(!unsafe { self.0.as_ref() }.started.replace(true), "a task started twice");
        unsafe { dispatch(self.0) };
    }
}
