// SPDX-License-Identifier: GPL-2.0

//! An async executor.
//!
//! A Task is a future plus a Header, the part the executor needs to run it
//! without knowing the future's type. A task lives in memory its owner
//! provides - a field of the object it's the task of, a bio's front pad, or a
//! box (box.rs) - and the executor never allocates or frees one. Its future is
//! initialized in place and never moves, so a large future is never built on
//! the spawner's stack. (An awaited future is the opposite: rustc builds it on
//! the poller's stack and moves it into the awaiting future, so large state
//! belongs in the task's own locals, with stack_pin_init!().)
//!
//! When the future returns, the executor calls the owner's done function.
//! That's the executor's last access to the task: the done function is where
//! the owner learns the task has finished, and may free it. Dropping a task
//! that started and hasn't finished panics.
//!
//! # Where a task runs
//!
//! A task is spawned on a workqueue, a RunQueue (queue.rs: run by its owner,
//! between the owner's own work), or a kthread of its own; a RunQueue or
//! kthread task always runs there. A workqueue task's awaits can say where it
//! resumes, with resume_on():
//! - Queue(q): on workqueue q.
//! - Inline: in the completer's context, an endio, where everything up to the
//!   next await runs. It must not sleep, or take a lock that isn't irqsave -
//!   C's continue_at(cl, fn, NULL), and as unchecked.
//! - Spawned, the default: where the task was spawned.
//! The target is recorded in the header (@target) while the await is pending.
//!
//! # Running and waiting
//!
//! A task is either running, or parked on exactly one Done (completion.rs),
//! and the header's @waiting says which: that Done's address, or null. Null is
//! every state in which someone owns the task - running, dispatched, not yet
//! started, or finished. @waiting is the task's lock: to run a parked task you
//! change @waiting from its Done to null, and only one party can, so a task is
//! never polled twice at once.
//!
//! An await of a Completion that isn't done names its Done in the poll's
//! Polled - lent to every poll through Context::ext() - and returns Pending.
//! An await waits on one thing: two named in one poll is a bug, and panics.
//! When the poll returns, run() publishes the Done in @waiting - not before,
//! because only then has every level of the state machine stored where it
//! resumes. The IO's completer marks the Done and calls wake(), which takes
//! @waiting from that Done to null if that's what it holds, and dispatches the
//! task. If the task isn't parked on that Done - it's running, and will see
//! the Done when it next looks, or it's parked on another - wake() writes
//! nothing.
//!
//! The two sides are the wait_event()/wake_up() protocol: run() stores
//! @waiting, then checks the Done; the completer stores the Done, then checks
//! @waiting. Each side's store must be ordered before its load, or both can
//! miss the other and the wakeup is lost. run()'s store is SeqCst, an xchg on
//! x86 as smp_store_mb() is; the completer's compare-exchange is a full
//! barrier by itself. If run() finds the Done completed after publishing, it
//! races the completer for @waiting with the same compare-exchange.
//!
//! Each side reads memory the other may already be finished with. Once
//! @waiting is published, a completer can take the task, run it to the end
//! and free it before run() checks the Done; once the Done is marked, the task
//! can see it, finish and be freed before the completer reads @waiting. So a
//! task - and so every Done in its memory - is freed only after an RCU grace
//! period, and both sides make that last access under rcu_read_lock().
//!
//! A poll finds its task through Context::ext() (current_task()), not through
//! a Waker. Nothing wakes a task through a Waker, so the Context's is
//! Waker::noop().
//!
//! A kthread task's thread runs the task whenever @waiting is null - which,
//! as the task runs nowhere else, means it's been dispatched there - and
//! sleeps otherwise. The done function runs on that thread, so the thread
//! exits on its own when the task finishes; the task's drop stops the thread
//! only if the task never started.
//!
//! Also here: completion.rs, the Done and Completion an IO's end is waited on
//! through; queue.rs, the RunQueue; box.rs, a task in a box of its own
//! (TaskBox) or sharing one with what it's the task of (BoxWithTask).

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

#[must_use = "a future does nothing unless awaited"]
#[pin_data(PinnedDrop)]
pub struct Resumed<'r, A> {
    #[pin]
    inner:    A,
    target:   Resume<'r>,
    recorded: Option<Recorded>,
    #[pin]
    _pin:     PhantomPinned,
}

#[derive(Clone, Copy)]
struct Recorded {
    task: TaskRef,
    ours: Target,
    prev: Target,
}

impl<A: Resumable> Future for Resumed<'_, A> {
    type Output = A::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<A::Output> {
        let this = self.project();

        let Some(task) = task_in(cx) else {
            return this.inner.poll_resume(cx);
        };

        let header = unsafe { task.0.as_ref() };
        let ours = match *this.target {
            Resume::Inline => Target::Inline,
            Resume::Queue(q) => Target::Queue(NonNull::from(q)),
            Resume::Spawned => return this.inner.poll_resume(cx),
        };
        match *this.recorded {
            None => *this.recorded = Some(Recorded { task, ours, prev: header.target.get() }),
            Some(recorded) => assert!(recorded.task == task, "an await moved from one task to another"),
        }
        header.target.set(ours);
        let result = this.inner.poll_resume(cx);
        if result.is_ready() {
            if let Some(recorded) = this.recorded.take() {
                if header.target.get() == recorded.ours {
                    header.target.set(recorded.prev);
                }
            }
        }
        result
    }
}

#[pinned_drop]
impl<A> PinnedDrop for Resumed<'_, A> {
    fn drop(self: Pin<&mut Self>) {
        if let Some(recorded) = self.recorded {
            let target = &unsafe { recorded.task.0.as_ref() }.target;
            if target.get() == recorded.ours {
                target.set(recorded.prev);
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Target {
    Inline,
    Queue(NonNull<Queue>),
}

impl Target {
    const UNUSED: Target = Target::Inline;
}

unsafe impl Send for Target {}

#[derive(Clone, Copy)]
pub enum Spawn<'q, 'n> {
    Queue(&'q Queue),
    Run(Pin<&'q RunQueue>),
    Thread(fmt::Arguments<'n>),
}

#[repr(C)]
#[pin_data]
struct Header {
    #[pin]
    work:      c::work_struct,
    waiting:   AtomicPtr<Done>,
    target:    Cell<Target>,
    runq:      Option<NonNull<RunQueue>>,
    thread:    Option<KthreadRef>,
    started:   Cell<bool>,
    finished:  Cell<bool>,
    runq_next: AtomicPtr<Header>,
    poll:      unsafe fn(NonNull<Header>, &mut Context<'_>) -> Poll<()>,
    finish:    unsafe fn(NonNull<Header>),
    _rcu:      RcuMember,
}

#[repr(C)]
#[pin_data(PinnedDrop)]
pub struct Task<'q, F: Future<Output = ()>> {
    #[pin]
    header:     Header,
    #[pin]
    future:     MaybeUninit<F>,
    owner_done: unsafe fn(NonNull<Self>),
    _q:         PhantomData<&'q Queue>,
}

unsafe impl<F: Future<Output = ()> + Send> Send for Task<'_, F> {}
unsafe impl<F: Future<Output = ()> + Send> Sync for Task<'_, F> {}

impl<'q, F: Future<Output = ()> + Send> Task<'q, F> {
    pub fn new<'n>(spawn: Spawn<'q, 'n>, future: impl PinInit<F> + 'n, owner_done: unsafe fn(NonNull<Self>))
        -> impl PinInit<Self, BchError> + 'n
    where
        F: 'n,
        'q: 'n,
    {
        let (target, runq) = match spawn {
            Spawn::Queue(wq) => (Target::Queue(NonNull::from(wq)), None),
            Spawn::Run(rq) => (Target::UNUSED, Some(NonNull::from(rq.get_ref()))),
            Spawn::Thread(_) => (Target::UNUSED, None),
        };
        let future = unsafe {
            pin_init::pin_init_from_closure(move |slot: *mut MaybeUninit<F>| {
                crate::util::alloc::init_at(future, slot.cast::<F>())
            })
        };
        pin_init!(Self {
            header <- pin_init!(Header {
                work <- crate::new_work!(crate::util::workqueue::run_raw::<Header>, "bcachefs task"),
                waiting:   AtomicPtr::new(null_mut()),
                target:    Cell::new(target),
                runq,
                thread:    None,
                started:   Cell::new(false),
                finished:  Cell::new(false),
                runq_next: AtomicPtr::new(null_mut()),
                poll:      Self::poll,
                finish:    Self::finish,
                _rcu:      RcuMember,
            }),
            future <- future,
            owner_done,
            _q:     PhantomData,
        }? BchError)
        .pin_chain(move |task| {
            if let Spawn::Thread(name) = spawn {
                let task_ref = task.task();
                let thread_fn = move || unsafe { task_ref.run_thread() };
                let thread = unsafe { KthreadRef::create(name, thread_fn) }?;
                *task.project().header.project().thread = Some(thread);
            }
            Ok(())
        })
    }

    pub fn future(self: Pin<&mut Self>) -> Pin<&mut F> {
        assert!(!self.header.started.get(), "a task's future, after it's started");
        unsafe { Pin::new_unchecked(self.get_unchecked_mut().future.assume_init_mut()) }
    }

    pub fn task(&self) -> TaskRef {
        TaskRef(NonNull::from(&self.header))
    }

    pub fn start(self: Pin<&Self>) {
        unsafe { self.task().start() };
    }

    unsafe fn poll(header: NonNull<Header>, cx: &mut Context<'_>) -> Poll<()> {
        let task = header.cast::<Self>().as_ptr();
        let future = unsafe { (&raw mut (*task).future).cast::<F>() };
        let result = unsafe { Pin::new_unchecked(&mut *future) }.poll(cx);
        if result.is_ready() {
            unsafe { core::ptr::drop_in_place(future) };
        }
        result
    }

    unsafe fn finish(header: NonNull<Header>) {
        let task = header.cast::<Self>();
        unsafe { ((*task.as_ptr()).owner_done)(task) };
    }
}

#[pinned_drop]
impl<F: Future<Output = ()>> PinnedDrop for Task<'_, F> {
    fn drop(self: Pin<&mut Self>) {
        let task = unsafe { self.get_unchecked_mut() };
        let started = task.header.started.get();
        assert!(!started || task.header.finished.get(), "a task freed while it runs");
        if !started {
            unsafe { core::ptr::drop_in_place(task.future.as_mut_ptr()) };
        }
        if let Some(thread) = task.header.thread.take() {
            if !started {
                thread.stop();
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct TaskRef(NonNull<Header>);

unsafe impl Send for TaskRef {}
unsafe impl Sync for TaskRef {}

impl TaskRef {
    pub unsafe fn start(self) {
        assert!(!unsafe { self.0.as_ref() }.started.replace(true), "a task started twice");
        unsafe { self.dispatch() };
    }

    unsafe fn dispatch(self) {
        let header = unsafe { self.0.as_ref() };
        if let Some(thread) = &header.thread {
            thread.wake();
            return;
        }
        if let Some(runq) = header.runq {
            unsafe { runq.as_ref().push(self.0) };
            return;
        }
        match header.target.get() {
            Target::Inline => {
                unsafe { self.run() };
            }
            Target::Queue(wq) => {
                unsafe { wq.as_ref().queue_work(&raw mut (*self.0.as_ptr()).work) };
            }
        }
    }

    unsafe fn run(self) -> bool {
        let header = unsafe { self.0.as_ref() };
        debug_assert!(header.waiting.load(Ordering::Relaxed).is_null() && !header.finished.get());
        let running_on = header.target.get();
        let never_moves = header.runq.is_some() || header.thread.is_some();
        let mut polled = Polled { task: self, parks_on: None };
        let mut cx = ContextBuilder::from_waker(Waker::noop()).ext(&mut polled).build();

        loop {
            if unsafe { (header.poll)(self.0, &mut cx) }.is_ready() {
                let finish = header.finish;
                header.finished.set(true);
                unsafe { finish(self.0) };
                return true;
            }

            let parked_on = polled_of(&mut cx).parks_on.take()
                .expect("a task pending on nothing: nothing will run it again");
            if !unsafe { parked_on.as_ref() }.done() {
                let _rcu = rcu::read_lock();
                header.waiting.store(parked_on.as_ptr(), Ordering::SeqCst);
                if !unsafe { parked_on.as_ref() }.done_ordered() {
                    return false;
                }
                let took_it_back = header.waiting
                    .compare_exchange(parked_on.as_ptr(), null_mut(), Ordering::SeqCst, Ordering::Relaxed)
                    .is_ok();
                if !took_it_back {
                    return false;
                }
            }

            if !never_moves && header.target.get() != running_on {
                unsafe { self.dispatch() };
                return false;
            }
        }
    }

    pub(super) unsafe fn wake(self, done: NonNull<Done>) {
        let header = unsafe { self.0.as_ref() };
        let was_parked_on_it = header.waiting
            .compare_exchange(done.as_ptr(), null_mut(), Ordering::SeqCst, Ordering::Relaxed)
            .is_ok();
        if was_parked_on_it {
            unsafe { self.dispatch() };
        }
    }

    unsafe fn run_thread(self) {
        if Kthread::should_stop() {
            return;
        }
        loop {
            let header = unsafe { self.0.as_ref() };
            let Ok(()) = sleep_until(Sleep::IDLE.freezable(),
                || header.waiting.load(Ordering::Acquire).is_null().then_some(()));
            let finished = unsafe { self.run() };
            if finished {
                return;
            }
        }
    }
}

impl RawWork for Header {
    const WORK: usize = core::mem::offset_of!(Header, work);

    #[inline(always)]
    unsafe fn run(header: NonNull<Self>) {
        unsafe { TaskRef(header).run() };
    }
}

pub fn current_task() -> impl Future<Output = TaskRef> {
    core::future::poll_fn(|cx| Poll::Ready(task_in(cx).expect("polled outside a task of ours")))
}

struct Polled {
    task:     TaskRef,
    parks_on: Option<NonNull<Done>>,
}

fn task_in(cx: &mut Context<'_>) -> Option<TaskRef> {
    cx.ext().downcast_mut::<Polled>().map(|polled| polled.task)
}

fn polled_of<'a>(cx: &'a mut Context<'_>) -> &'a mut Polled {
    cx.ext().downcast_mut::<Polled>().expect("polled outside a task of ours")
}

fn park_on(cx: &mut Context<'_>, done: &Done) {
    let already = polled_of(cx).parks_on.replace(NonNull::from(done));
    assert!(already.is_none(), "two awaits pending in one poll: a task waits on one thing at a time");
}
