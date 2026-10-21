// SPDX-License-Identifier: GPL-2.0

//! An async executor: spawn() a future as a task on a workqueue, and await
//! its JoinHandle - or join_sync() it from a thread - for what it returns.
//!
//! Every await says where its task resumes: what we await - a Gather's
//! wait(), a JoinHandle - isn't a Future until .resume_on() makes it one:
//!
//!     let ret = g.as_mut().wait().resume_on(Resume::Queue(wq)).await;
//!
//! Resume::Queue resumes the task on that workqueue; Resume::Thread on the
//! task's own kthread - spawn_thread()'s, kept for its life, for work that
//! blocks, seen in ps by its name. Resume::Inline resumes
//! it right where it's woken - in the waker's context, an endio's - as C's
//! continue_at(cl, fn, NULL) does: no hop through a workqueue, for a
//! continuation that's cheap and safe there. What runs inline is everything
//! up to the next await, and the drops of the task's locals if it ends
//! there: none of it may sleep, or take a lock that isn't irqsave - as for
//! a C continuation with no workqueue, unchecked.
//!
//! How: the Waker a task is polled with points at it. Inline is a tag bit
//! in that pointer, so each wake carries its own - under join!(), a wake
//! that isn't inline never resumes the task in an endio. A Queue is
//! recorded in the task by the await, while it's pending: a wake without
//! the tag dispatches there - or, for a future we didn't write, awaited
//! without resume_on(), to where it was spawned. A thread's dispatch is a
//! flag and wake_up_process(): safe from an endio, as queue_work() is.
//!
//! A task's state is one word: SCHEDULED, RUNNING, NOTIFIED, COMPLETE, and
//! a refcount. It's what keeps a task from being polled twice at once - not
//! the workqueue's non-reentrancy, which an inline wake doesn't go through.
//! A wake that finds it idle schedules it; one that finds it running marks
//! it NOTIFIED, and the poll in progress repolls when it's done - in place,
//! if the wake was inline or the poll is running where the task's to
//! resume; otherwise it dispatches it there.
//!
//! A task holds a ref on itself until it completes: dropping its Wakers
//! never drops it, so its future is never dropped from a waker's context. A
//! task pending forever with nothing to wake it - a lost wakeup - is leaked,
//! not freed. Its memory is kmalloc()'d: the last ref may be dropped in an
//! endio, and kfree() is safe anywhere.
//!
//! What a task returns is the JoinHandle's; dropped, the task's detached,
//! and what it returns is dropped when it completes, in its context. The
//! JoinHandle's wait is a Gather - the task holds one ref, put when it
//! completes.

use crate::c;
use crate::errcode::{errptr_to_result, BchError};
use crate::util::alloc::{kalloc_pinned, kfree_pinned, AllocError};
use crate::util::gather::{Gather, GatherRef};
use crate::util::kthread::Kthread;
use crate::util::workqueue::Queue;
use crate::util::Printbuf;
use core::cell::UnsafeCell;
use core::ffi::{c_int, c_void};
use core::fmt;
use core::future::Future;
use core::mem::{ManuallyDrop, MaybeUninit};
use core::pin::Pin;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use pin_init::{pin_data, pin_init, PinInit};

/// Where a task resumes, once what it awaits is done.
#[derive(Clone, Copy)]
pub enum Resume<'a> {
    /// In the waker's context - an endio's, perhaps: see the module notes.
    Inline,
    Queue(&'a Queue),
    /// On the task's own kthread: a task spawn_thread() made.
    Thread,
}

/// What's awaited once it's said where to resume: resume_on() makes it a
/// Future. Ours - Gather::wait(), JoinHandle - aren't Futures otherwise; any
/// Future is one too, so resume_on() is how every await reads.
pub trait Resumable {
    type Output;

    fn poll_resume(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output>;

    fn resume_on(self, r: Resume<'_>) -> Resumed<'_, Self>
    where
        Self: Sized,
    {
        Resumed { inner: self, target: r, recorded: None }
    }
}

impl<F: Future> Resumable for F {
    type Output = F::Output;

    fn poll_resume(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        Future::poll(self, cx)
    }
}

/// A Resumable, awaitable: resume_on()'s.
#[must_use = "a future does nothing unless awaited"]
pub struct Resumed<'r, A> {
    inner:    A,
    target:   Resume<'r>,
    /// The task, and where we recorded it's to resume, to clear when we're
    /// done.
    recorded: Option<(NonNull<Header>, usize)>,
}

// SAFETY: @recorded is a task's header - atomics - and only touched while
// the task's alive, which we're part of
unsafe impl<A: Send> Send for Resumed<'_, A> {}
unsafe impl<A: Sync> Sync for Resumed<'_, A> {}

impl<A: Resumable> Future for Resumed<'_, A> {
    type Output = A::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<A::Output> {
        // SAFETY: @inner is structurally pinned - never moved out, dropped in
        // place - and the rest isn't
        let this = unsafe { self.get_unchecked_mut() };
        let inner = unsafe { Pin::new_unchecked(&mut this.inner) };

        // Not one of our tasks - block_on()'s, say: nowhere to record it
        let Some(task) = header_of(cx.waker()) else {
            return inner.poll_resume(cx);
        };

        // SAFETY: we're being polled by it: it's alive
        let hr = unsafe { task.as_ref() };
        let tag = match this.target {
            Resume::Inline => INLINE,
            r => {
                let at = match r {
                    Resume::Queue(q) => core::ptr::from_ref(q) as usize,
                    Resume::Thread => {
                        assert!(!hr.thread.is_null(), "Resume::Thread in a task without a thread: spawn_thread() it");
                        THREAD
                    }
                    Resume::Inline => unreachable!(),
                };
                hr.next.store(at, Ordering::Release);
                this.recorded = Some((task, at));
                0
            }
        };
        // SAFETY: borrowed for the poll, never dropped: a clone is a ref
        let waker = ManuallyDrop::new(unsafe { waker_raw(task, tag) });
        inner.poll_resume(&mut Context::from_waker(&waker))
    }
}

impl<A> Drop for Resumed<'_, A> {
    fn drop(&mut self) {
        // Done with: an untagged wake mustn't find our queue - it's only
        // borrowed - unless an await since has recorded it again
        if let Some((task, q)) = self.recorded {
            // SAFETY: we're in its future: it's alive
            let _ = unsafe { task.as_ref() }.next.compare_exchange(q, 0, Ordering::Release,
                                                                    Ordering::Relaxed);
        }
    }
}

/* The state word: */
/// Dispatched - queued, or being run inline - and not yet running.
const SCHEDULED:       usize = 1 << 0;
const RUNNING:         usize = 1 << 1;
/// Woken while being polled, by a wake that wasn't inline.
const NOTIFIED:        usize = 1 << 2;
/// Woken while being polled, inline.
const NOTIFIED_INLINE: usize = 1 << 3;
const COMPLETE:        usize = 1 << 4;
/// The JoinHandle's still there: what's returned is its.
const JOIN:            usize = 1 << 5;
/// One ref: the task's own until it completes, the JoinHandle's, each Waker's.
const REF:             usize = 1 << 6;

/// The Waker tag for Resume::Inline: tasks are aligned well past it.
const INLINE: usize = 1;

/// Where a task's to resume, or was spawned, or is: a Queue's address, or
/// this - no Queue's - for its kthread.
const THREAD: usize = 1;

/// What every task is, whatever its future: its work first, so a
/// work_struct pointer is one to it.
#[repr(C)]
struct Header {
    work:     c::work_struct,
    state:    AtomicUsize,
    /// Where a wake that isn't inline resumes it: what the await in
    /// progress recorded, or 0 for @spawned.
    next:     AtomicUsize,
    /// A 'static Queue, or THREAD.
    spawned:  usize,
    /// Its kthread, if spawn_thread() made it, and a ref on it.
    thread:   *mut c::task_struct,
    /// Dispatched to its thread: the thread runs it.
    thread_go: core::sync::atomic::AtomicBool,
    vtable:   &'static TaskVTable,
}

/// A task's own, by its future's type.
struct TaskVTable {
    /// Poll the future once; Ready, the task's complete - and maybe freed.
    poll:    unsafe fn(NonNull<Header>, &mut Context<'_>) -> Poll<()>,
    /// Drop and free it, never started: Unstarted's Drop.
    discard: unsafe fn(NonNull<Header>),
    dealloc: unsafe fn(NonNull<Header>),
}

/// The header and what a JoinHandle<T> reads, without the future's type.
#[repr(C)]
#[pin_data]
struct TaskHead<T> {
    header: Header,
    /// The JoinHandle's wait: put once it's done.
    #[pin]
    done:   Gather<()>,
    /// The task's ref on @done.
    done_ref: UnsafeCell<Option<GatherRef<()>>>,
    output: UnsafeCell<MaybeUninit<T>>,
}

#[repr(C)]
#[pin_data]
struct Task<F: Future> {
    #[pin]
    head:   TaskHead<F::Output>,
    future: UnsafeCell<ManuallyDrop<F>>,
}

impl<F: Future> Task<F> {
    const VTABLE: TaskVTable = TaskVTable {
        poll:    Self::poll,
        discard: Self::discard,
        dealloc: Self::dealloc,
    };

    /// # Safety
    /// @h is a Task<F>'s, and we're its one poller: RUNNING is ours.
    unsafe fn poll(h: NonNull<Header>, cx: &mut Context<'_>) -> Poll<()> {
        let task = h.cast::<Self>().as_ptr();
        // SAFETY: the caller's; pinned where it was made
        let future = unsafe { Pin::new_unchecked(&mut **(*task).future.get()) };
        match future.poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(v) => {
                // SAFETY: ours; never polled again
                unsafe {
                    (*(*task).head.output.get()).write(v);
                    ManuallyDrop::drop(&mut *(*task).future.get());
                    Self::complete(h);
                }
                Poll::Ready(())
            }
        }
    }

    /// The future returned: its output's the JoinHandle's - or dropped, if
    /// there's none - the JoinHandle's woken, and the task's own ref dropped.
    ///
    /// # Safety
    /// @h is a Task<F>'s, RUNNING ours, and its output just stored.
    unsafe fn complete(h: NonNull<Header>) {
        let task = h.cast::<Self>().as_ptr();
        // SAFETY: the caller's
        let hr = unsafe { h.as_ref() };

        let mut s = hr.state.load(Ordering::Acquire);
        loop {
            let new = (s & !(RUNNING|NOTIFIED|NOTIFIED_INLINE)) | COMPLETE;
            match hr.state.compare_exchange_weak(s, new, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_)    => break,
                Err(cur) => s = cur,
            }
        }
        if s & JOIN == 0 {
            // SAFETY: complete, and the JoinHandle's gone: nobody takes it
            unsafe { (*(*task).head.output.get()).assume_init_drop() };
        }

        // After COMPLETE: a JoinHandle woken by it, its output taken and it
        // dropped, must find it complete - else we'd drop the output too
        // SAFETY: the task's ref, taken once
        unsafe { drop((*(*task).head.done_ref.get()).take()) };
        if !hr.thread.is_null() {
            // done: its thread exits - it has its own ref
            // SAFETY: the task's ref on its thread
            unsafe { c::bch2_wake_up_process(hr.thread) };
        }
        // SAFETY: the task's own ref
        unsafe { put_ref(h) };
    }

    /// # Safety
    /// @h is a Task<F>'s, made and never started, and nobody else's.
    unsafe fn discard(h: NonNull<Header>) {
        let task = h.cast::<Self>().as_ptr();
        // SAFETY: the caller's; its thread, if it has one, never woken: stopped,
        // it exits without touching the task
        unsafe {
            let thread = (*task).head.header.thread;
            if !thread.is_null() {
                c::bch2_kthread_stop(thread);
                c::bch2_put_task_struct(thread);
            }
            ManuallyDrop::drop(&mut *(*task).future.get());
            drop((*(*task).head.done_ref.get()).take());
            kfree_pinned(task);
        }
    }

    /// # Safety
    /// @h is a Task<F>'s, and its last ref's gone.
    unsafe fn dealloc(h: NonNull<Header>) {
        // SAFETY: the caller's; kalloc_pinned()'s
        unsafe { kfree_pinned(h.cast::<Self>().as_ptr()) }
    }
}

/// Spawn @fut on @wq - first polled there, and resumed there by a wake
/// that hasn't said where - for its JoinHandle.
pub fn spawn<F>(wq: &'static Queue, fut: F) -> Result<JoinHandle<F::Output>, BchError>
where
    F: Future + Send + 'static,
    F::Output: Send,
{
    // SAFETY: 'static: it borrows nothing
    Ok(unsafe { spawn_unstarted(Spawn::Queue(wq), fut) }?.start())
}

/// Spawn @fut on a kthread of its own, named @name - first polled there,
/// and resumed there by Resume::Thread, or a wake that hasn't said where.
/// The thread's the task's till it's done, sleeping while it runs elsewhere.
pub fn spawn_thread<F>(name: fmt::Arguments<'_>, fut: F) -> Result<JoinHandle<F::Output>, BchError>
where
    F: Future + Send + 'static,
    F::Output: Send,
{
    // SAFETY: 'static: it borrows nothing
    Ok(unsafe { spawn_unstarted(Spawn::Thread(name), fut) }?.start())
}

/// Where a spawned task's first polled, and resumed by a wake that hasn't
/// said where: spawn()'s, or spawn_thread()'s.
pub enum Spawn<'a> {
    Queue(&'a Queue),
    /// A kthread of its own, named.
    Thread(fmt::Arguments<'a>),
}

/// A task made, to be started - or, dropped, discarded: its future dropped
/// unpolled, its thread stopped unwoken.
pub struct Unstarted<T> {
    head: NonNull<TaskHead<T>>,
}

// SAFETY: as JoinHandle's: what it holds is a task, its output a T
unsafe impl<T: Send> Send for Unstarted<T> {}

impl<T> Unstarted<T> {
    /// Dispatched, for its JoinHandle.
    pub fn start(self) -> JoinHandle<T> {
        let head = ManuallyDrop::new(self).head;
        // SAFETY: made, not started: ours to dispatch
        unsafe { dispatch(NonNull::new_unchecked(&raw mut (*head.as_ptr()).header), false) };
        JoinHandle { head, wait: None, taken: false }
    }
}

impl<T> Drop for Unstarted<T> {
    fn drop(&mut self) {
        // SAFETY: made, not started, and ours alone
        unsafe {
            let h = NonNull::new_unchecked(&raw mut (*self.head.as_ptr()).header);
            (h.as_ref().vtable.discard)(h);
        }
    }
}

/// A task for @fut, made - its thread too, if it's to have one - to be
/// started.
///
/// # Safety
/// What @fut borrows, and @at's Queue, outlive the task: whoever made it
/// joins it - or drops it unstarted - before they're gone. spawn() and
/// spawn_thread() are this for what's 'static.
pub unsafe fn spawn_unstarted<F>(at: Spawn<'_>, fut: F) -> Result<Unstarted<F::Output>, BchError>
where
    F: Future + Send,
    F::Output: Send,
{
    let task = match at {
        Spawn::Queue(wq) => make(core::ptr::from_ref(wq) as usize, fut)?,
        Spawn::Thread(name) => {
            let mut name_buf = Printbuf::new();
            write!(name_buf, "{name}");

            let task = make(THREAD, fut)?;
            // SAFETY: just made; the thread's data, from when it's woken - and
            // if it's stopped unwoken, it doesn't touch it
            let thread = unsafe {
                let h = &raw mut (*task).head.header;
                c::bch2_kthread_create(Some(run_thread), h.cast(), name_buf.as_c_str().as_ptr().cast())
            };
            match errptr_to_result(NonNull::new(thread).expect("kthread_create() gives a task or an error")) {
                // SAFETY: just made, and nobody else has it
                Err(e) => unsafe {
                    Task::<F>::discard(NonNull::new_unchecked(&raw mut (*task).head.header));
                    return Err(e);
                },
                // SAFETY: a ref on the thread, the task's; and one on the task,
                // the thread's; both before it can run
                Ok(thread) => unsafe {
                    c::bch2_get_task_struct(thread.as_ptr());
                    let h = &raw mut (*task).head.header;
                    (*h).thread = thread.as_ptr();
                    (*h).state.fetch_add(REF, Ordering::Relaxed);
                    task
                },
            }
        }
    };
    // SAFETY: just made
    Ok(Unstarted { head: unsafe { NonNull::new_unchecked(&raw mut (*task).head) } })
}

/// A task made, not yet started: its own ref and its JoinHandle's.
fn make<F>(spawned: usize, fut: F) -> Result<*mut Task<F>, AllocError>
where
    F: Future + Send,
    F::Output: Send,
{
    let init = pin_init!(Task::<F> {
        head <- pin_init!(TaskHead {
            header: Header {
                // SAFETY: plain C; bch2_init_work() below initializes it
                work:      unsafe { core::mem::zeroed() },
                // dispatched by start()
                state:     AtomicUsize::new(2 * REF | JOIN | SCHEDULED),
                next:      AtomicUsize::new(0),
                spawned,
                thread:    core::ptr::null_mut(),
                thread_go: core::sync::atomic::AtomicBool::new(false),
                vtable:    &Task::<F>::VTABLE,
            },
            done <- Gather::new(),
            done_ref: UnsafeCell::new(None),
            output: UnsafeCell::new(MaybeUninit::uninit()),
        }),
        future: UnsafeCell::new(ManuallyDrop::new(fut)),
    });
    // kmalloc()'d: the last ref may go in an endio. Its error - there's
    // none - made the allocation's
    // SAFETY: @init's own
    let init = unsafe {
        pin_init::pin_init_from_closure(move |slot| {
            #[cfg(bch_pin_init_v2)]
            let res = init.__init(slot);
            #[cfg(not(bch_pin_init_v2))]
            let res = init.__pinned_init(slot);
            res.map_err(|e: core::convert::Infallible| match e {})
        })
    };
    let task = kalloc_pinned::<Task<F>, AllocError>(init)?;

    // SAFETY: just made: nobody else has it
    unsafe {
        let head = &raw mut (*task).head;
        *(*head).done_ref.get() = Some((*head).done.get());
        c::bch2_init_work(&raw mut (*head).header.work, Some(run_work));
    }
    Ok(task)
}

/* Running a task: */

/// Where a poll is running.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Here {
    /// In a waker's context.
    Inline,
    /// On a Queue, by its address, or the task's thread: THREAD.
    At(usize),
}

/// Where a wake that isn't inline resumes it.
fn target(h: &Header) -> usize {
    match h.next.load(Ordering::Acquire) {
        0 => h.spawned,
        q => q,
    }
}

/// Run it, just SCHEDULED by us: polled here, if @inline, or queued.
///
/// # Safety
/// @h is a live task's, and SCHEDULED by the caller.
unsafe fn dispatch(h: NonNull<Header>, inline: bool) {
    // SAFETY: the caller's
    let hr = unsafe { h.as_ref() };
    if inline {
        // SAFETY: the caller's
        unsafe { run(h, Here::Inline) };
    } else {
        let at = target(hr);
        if at == THREAD {
            hr.thread_go.store(true, Ordering::Release);
            // SAFETY: the task's ref on its thread
            unsafe { c::bch2_wake_up_process(hr.thread) };
        } else {
            // SAFETY: a queue an await recorded, borrowed till it's done, or
            // the task's 'static one; the work's the task's, alive till it's
            // done - and SCHEDULED, so not on a queue already
            unsafe { (*(at as *const Queue)).queue_work(h.as_ptr().cast()) };
        }
    }
}

unsafe extern "C" fn run_work(work: *mut c::work_struct) {
    // SAFETY: a task's work, first in it, queued by dispatch() - to where
    // it's to resume, which only a poll changes, and it's not been polled
    let h = unsafe { NonNull::new_unchecked(work.cast::<Header>()) };
    let here = Here::At(target(unsafe { h.as_ref() }));
    unsafe { run(h, here) };
}

/// A task's kthread: runs it when it's dispatched here, till it's done -
/// wherever that is. Holds a ref on it.
unsafe extern "C" fn run_thread(data: *mut c_void) -> c_int {
    // Discarded, stopped before it was woken: the task's not ours to touch -
    // in userspace, a stopped kthread runs anyway
    if Kthread::should_stop() {
        return 0;
    }
    // SAFETY: spawn_unstarted()'s, and our ref keeps it
    let h = unsafe { NonNull::new_unchecked(data.cast::<Header>()) };
    let hr = unsafe { h.as_ref() };
    loop {
        // the state set before the checks: a dispatch or completion after
        // them wakes us
        unsafe { c::bch2_set_current_state_idle(false) };
        if hr.thread_go.swap(false, Ordering::AcqRel) {
            unsafe { c::bch2_set_current_state_running() };
            // SAFETY: dispatched here: SCHEDULED for us
            unsafe { run(h, Here::At(THREAD)) };
            continue;
        }
        if hr.state.load(Ordering::Acquire) & COMPLETE != 0 {
            break;
        }
        unsafe { c::bch2_schedule() };
    }
    unsafe { c::bch2_set_current_state_running() };
    // SAFETY: the thread's ref
    unsafe { put_ref(h) };
    0
}

/// Poll the task, until it's done or idle - or wants to run elsewhere.
///
/// # Safety
/// @h is a live task's, and SCHEDULED for us.
unsafe fn run(h: NonNull<Header>, here: Here) {
    // SAFETY: the caller's
    let hr = unsafe { h.as_ref() };

    let mut s = hr.state.load(Ordering::Acquire);
    loop {
        debug_assert!(s & (SCHEDULED|RUNNING|COMPLETE) == SCHEDULED);
        let new = (s & !(SCHEDULED|NOTIFIED|NOTIFIED_INLINE)) | RUNNING;
        match hr.state.compare_exchange_weak(s, new, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_)    => break,
            Err(cur) => s = cur,
        }
    }

    loop {
        // SAFETY: borrowed for the poll: a clone is a ref
        let waker = ManuallyDrop::new(unsafe { waker_raw(h, 0) });
        // SAFETY: RUNNING is ours; Ready, it's complete, and may be gone
        if unsafe { (hr.vtable.poll)(h, &mut Context::from_waker(&waker)) }.is_ready() {
            return;
        }

        // Pending: idle, unless woken meanwhile
        let mut s = hr.state.load(Ordering::Acquire);
        let repoll = loop {
            let (new, repoll) = if s & NOTIFIED != 0 {
                if here == Here::At(target(hr)) {
                    (s & !(NOTIFIED|NOTIFIED_INLINE), true)
                } else {
                    // to run where it's to resume, not here
                    ((s & !(RUNNING|NOTIFIED|NOTIFIED_INLINE)) | SCHEDULED, false)
                }
            } else if s & NOTIFIED_INLINE != 0 {
                (s & !NOTIFIED_INLINE, true)
            } else {
                (s & !RUNNING, false)
            };
            match hr.state.compare_exchange_weak(s, new, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) if repoll => break true,
                Ok(_) => {
                    if new & SCHEDULED != 0 {
                        // SAFETY: SCHEDULED by us
                        unsafe { dispatch(h, false) };
                    }
                    break false;
                }
                Err(cur) => s = cur,
            }
        };
        if !repoll {
            return;
        }
    }
}

/// # Safety
/// @h is a live task's, and the caller has a ref to put.
unsafe fn put_ref(h: NonNull<Header>) {
    // SAFETY: the caller's
    let hr = unsafe { h.as_ref() };
    if hr.state.fetch_sub(REF, Ordering::AcqRel) & !(REF - 1) == REF {
        if !hr.thread.is_null() {
            // SAFETY: the task's ref on its thread - exited, or exiting
            unsafe { c::bch2_put_task_struct(hr.thread) };
        }
        // SAFETY: the last ref
        unsafe { (hr.vtable.dealloc)(h) };
    }
}

/// Wake it: schedule it to run, if it's idle - inline, here, if @inline.
///
/// # Safety
/// @h is a live task's.
unsafe fn wake(h: NonNull<Header>, inline: bool) {
    // SAFETY: the caller's
    let hr = unsafe { h.as_ref() };

    let mut s = hr.state.load(Ordering::Acquire);
    loop {
        let new = if s & (COMPLETE|SCHEDULED) != 0 {
            return;
        } else if s & RUNNING != 0 {
            s | if inline { NOTIFIED_INLINE } else { NOTIFIED }
        } else {
            s | SCHEDULED
        };
        match hr.state.compare_exchange_weak(s, new, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => {
                if new & SCHEDULED != 0 {
                    // SAFETY: SCHEDULED by us; our caller's ref keeps it
                    // alive, and so does its own till it's complete
                    unsafe { dispatch(h, inline) };
                }
                return;
            }
            Err(cur) => s = cur,
        }
    }
}

/* Wakers: a task pointer, tagged INLINE or not. */

static VTABLE: RawWakerVTable = RawWakerVTable::new(waker_clone, waker_wake, waker_wake_by_ref, waker_drop);

/// The task a Waker of ours wakes, untagged.
fn header_of(w: &Waker) -> Option<NonNull<Header>> {
    core::ptr::eq(w.vtable(), &VTABLE).then(|| untag(w.data()).0)
}

/// # Safety
/// @h is a live task's; the Waker owns a ref - or, borrowed, never drops.
unsafe fn waker_raw(h: NonNull<Header>, tag: usize) -> Waker {
    // SAFETY: the vtable's contract: below
    unsafe { Waker::from_raw(RawWaker::new((h.as_ptr() as usize | tag) as *const (), &VTABLE)) }
}

fn untag(p: *const ()) -> (NonNull<Header>, bool) {
    let p = p as usize;
    (NonNull::new((p & !INLINE) as *mut Header).unwrap(), p & INLINE != 0)
}

unsafe fn waker_clone(p: *const ()) -> RawWaker {
    let (h, _) = untag(p);
    // SAFETY: a Waker's: the task's alive
    unsafe { h.as_ref() }.state.fetch_add(REF, Ordering::Relaxed);
    RawWaker::new(p, &VTABLE)
}

unsafe fn waker_wake(p: *const ()) {
    // SAFETY: an owned Waker: woken, then its ref put
    unsafe {
        waker_wake_by_ref(p);
        waker_drop(p);
    }
}

unsafe fn waker_wake_by_ref(p: *const ()) {
    let (h, inline) = untag(p);
    // SAFETY: a Waker's: the task's alive
    unsafe { wake(h, inline) };
}

unsafe fn waker_drop(p: *const ()) {
    // SAFETY: an owned Waker's ref
    unsafe { put_ref(untag(p).0) };
}

/* JoinHandle */

/// A spawned task's: what it returns. Dropped, the task's detached.
pub struct JoinHandle<T> {
    head:  NonNull<TaskHead<T>>,
    /// An await in progress.
    wait:  Option<crate::util::gather::Wait<'static, ()>>,
    taken: bool,
}

// SAFETY: what it reads is the task's output, a T, from wherever it was
// made; the task's header is atomics
unsafe impl<T: Send> Send for JoinHandle<T> {}

impl<T> JoinHandle<T> {
    fn done(&mut self) -> Pin<&'static mut Gather<()>> {
        // SAFETY: pinned in the task, which our ref keeps; ours alone, as the
        // JoinHandle is
        unsafe { Pin::new_unchecked(&mut (*self.head.as_ptr()).done) }
    }

    /// # Safety
    /// The task's complete: @done waited on.
    unsafe fn take(&mut self) -> T {
        self.taken = true;
        // SAFETY: the caller's; it's only taken once, here
        unsafe { (*(*self.head.as_ptr()).output.get()).assume_init_read() }
    }

    /// Wait for what it returns, sleeping TASK_IDLE: for a thread, not a
    /// task.
    pub fn join_sync(self) -> T {
        self.join_sync_state(false)
    }

    /// join_sync(), freezable: for a task that freezes in place, whose
    /// joiner must too.
    pub fn join_sync_freezable(self) -> T {
        self.join_sync_state(true)
    }

    fn join_sync_state(mut self, freezable: bool) -> T {
        // an await of it begun: its borrow of the wait ended first
        self.wait = None;
        if freezable {
            self.done().wait_sync_freezable();
        } else {
            self.done().wait_sync();
        }
        // SAFETY: waited on
        unsafe { self.take() }
    }
}

impl<T> Resumable for JoinHandle<T> {
    type Output = T;

    fn poll_resume(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        let this = self.get_mut();
        if this.wait.is_none() {
            this.wait = Some(this.done().wait());
        }
        // SAFETY: @wait's in its Option, not moved while it's there
        let wait = unsafe { Pin::new_unchecked(this.wait.as_mut().unwrap()) };
        match wait.poll_resume(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(()) => {
                this.wait = None;
                // SAFETY: waited on
                Poll::Ready(unsafe { this.take() })
            }
        }
    }
}

impl<T> Drop for JoinHandle<T> {
    fn drop(&mut self) {
        // its Waker taken back, before the task can be freed
        self.wait = None;

        // SAFETY: our ref keeps it
        let h = unsafe { NonNull::new_unchecked(&raw mut (*self.head.as_ptr()).header) };
        let hr = unsafe { h.as_ref() };

        let mut s = hr.state.load(Ordering::Acquire);
        loop {
            match hr.state.compare_exchange_weak(s, s & !JOIN, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_)    => break,
                Err(cur) => s = cur,
            }
        }
        if s & COMPLETE != 0 && !self.taken {
            // complete before we let go: what it returned is ours to drop
            // SAFETY: complete, and not taken
            unsafe { (*(*self.head.as_ptr()).output.get()).assume_init_drop() };
        }
        // SAFETY: our ref
        unsafe { put_ref(h) };
    }
}
