// SPDX-License-Identifier: GPL-2.0
//! An async executor on a workqueue: a task is a future and an embedded
//! work_struct. The workqueue runs the task, which polls the future once; its
//! waker re-enqueues it to be polled again. Ported from Wedson Almeida Filho's
//! out-of-tree kasync onto kernel::workqueue and the shim's matching API.
//!
//! Two workqueue properties, doing different jobs:
//!
//! - The pending bit dedups enqueues of a task queued and not yet running: the
//!   second enqueue fails, as Task::wake() wants. It doesn't serialize runs -
//!   it's cleared before the work function runs, so a wake mid-poll requeues.
//! - Non-reentrancy serializes runs: a workqueue never runs one work item on
//!   two workers at once (process_one_work(), __queue_work(); in userspace
//!   find_runnable_work()). This, not the pending bit, is what makes the
//!   UnsafeCell access in Task::poll() and the Sync impl sound.
//!
//! So a task is polled by one thread at a time, and a wake during a poll
//! yields exactly one more poll.
//!
//! WaitGroup is a fork-join barrier - the last task to finish wakes the waiter
//! - and block_on() drives a future from a synchronous caller, parking the
//! thread: a completion in the kernel, a condvar in userspace, the one
//! platform-specific piece.
//!
//! Both properties are tested in src/workqueue_test.rs, in the tools crate - a
//! bcachefs-kernel test binary has no queue_work(). Run it after changing how a
//! task is enqueued or polled.

use core::cell::UnsafeCell;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

#[cfg(kernel)]
use kernel::{
    prelude::*,
    sync::Arc,
    workqueue::{Queue, Work, WorkItem},
};

#[cfg(not(kernel))]
use bcachefs_shim::workqueue::{AllocError, Queue, Work, WorkItem};
#[cfg(not(kernel))]
use pin_init::pin_data;
#[cfg(not(kernel))]
use std::sync::{Arc, Condvar, Mutex};

#[pin_data]
struct Task<F: Future<Output = ()> + Send + 'static> {
    #[pin]
    work: Work<Task<F>>,
    queue: &'static Queue,
    future: UnsafeCell<F>,
}

// SAFETY: workqueue non-reentrancy (see the module header) means `run` - the
// only place the future is touched - executes on one thread at a time, so the
// future is never polled concurrently; the task is `Sync` whenever the future
// is `Send`.
unsafe impl<F: Future<Output = ()> + Send + 'static> Sync for Task<F> {}

impl<F: Future<Output = ()> + Send + 'static> Task<F> {
    /// Poll the future once, on a workqueue thread.
    fn poll(self: Arc<Self>) {
        let waker = waker_for(self.clone());
        let mut cx = Context::from_waker(&waker);

        // SAFETY: workqueue non-reentrancy serializes runs of this work item, so
        // access is exclusive here, and the future never moves (it lives behind
        // `Arc`).
        let future = unsafe { Pin::new_unchecked(&mut *self.future.get()) };

        // Ready: nothing re-enqueues us, the Arc refs drain, the task frees.
        // Pending: the future kept the waker; waking it re-enqueues a poll.
        let _: Poll<()> = future.poll(&mut cx);
    }

    /// Re-run this task on its workqueue (the waker calls this).
    fn wake(self: Arc<Self>) {
        let queue = self.queue;
        // Err: already enqueued, so a poll is pending - what we want, not a failure
        #[allow(clippy::let_unit_value)]
        let _ = queue.enqueue(self);
    }
}

// ---- WorkItem: poll dispatch from the workqueue ----
// The two trait shapes differ (kernel: associated `Pointer` + macro-provided
// offset; shim: const offset), so the impl is cfg-split; both just call poll().

// SAFETY: `WORK_OFFSET` is the offset of the `work` field — an embedded
// `Work<Self>` — within `Self`.
#[cfg(not(kernel))]
unsafe impl<F: Future<Output = ()> + Send + 'static> WorkItem for Task<F> {
    const WORK_OFFSET: usize = core::mem::offset_of!(Self, work);

    fn run(self: Arc<Self>) {
        self.poll();
    }
}

#[cfg(kernel)]
impl<F: Future<Output = ()> + Send + 'static> WorkItem for Task<F> {
    type Pointer = Arc<Self>;

    fn run(this: Arc<Self>) {
        this.poll();
    }
}

// HasWork through impl_has_work!, as the kernel's own ClosureWork<T>; work is
// the first field, so container_of is identity.
#[cfg(kernel)]
kernel::impl_has_work! {
    impl{F: Future<Output = ()> + Send + 'static} HasWork<Self> for Task<F> { self.work }
}

/// Spawn @future onto @queue, fire and forget: pair it with a WaitGroup to learn
/// when a batch is done. A Result in both builds, though userspace can't fail,
/// so callers stay cfg-free.
#[cfg(not(kernel))]
pub fn spawn<F: Future<Output = ()> + Send + 'static>(
    queue: &'static Queue,
    future: F,
) -> Result<(), AllocError> {
    let task = Arc::new(Task {
        work: Work::new(),
        queue,
        future: UnsafeCell::new(future),
    });
    queue.enqueue(task);
    Ok(())
}

// XXX(kernel): Work::new()'s arguments (name, lock class key) and the
// Arc::pin_init shape aren't verified against the kernel crate.
#[cfg(kernel)]
pub fn spawn<F: Future<Output = ()> + Send + 'static>(
    queue: &'static Queue,
    future: F,
) -> Result<()> {
    let task = Arc::pin_init(
        pin_init!(Task {
            work <- kernel::new_work!("Task::work"),
            queue,
            future: UnsafeCell::new(future),
        }),
        GFP_KERNEL,
    )?;
    // Err means already enqueued, and this Task was constructed a line ago -
    // its work_struct cannot be on a queue yet, so there is no Err to see.
    let _ = queue.enqueue(task);
    Ok(())
}

/// The system-wide unbound workqueue — the default executor for [`spawn`].
#[cfg(not(kernel))]
pub fn system_unbound() -> &'static Queue {
    bcachefs_shim::workqueue::system_unbound()
}

#[cfg(kernel)]
pub fn system_unbound() -> &'static Queue {
    kernel::workqueue::system_unbound()
}

// ---- WaitGroup: an async fork-join barrier ----

/// A counting fork-join barrier: @n tasks each call done(), and wait() resolves
/// after the last - whose wake can unpark a thread in block_on().
pub struct WaitGroup {
    remaining: AtomicU32,
    waker: WakerSlot,
}

impl WaitGroup {
    /// Shared by the workers and the waiter. A Result in both builds, as spawn().
    #[cfg(not(kernel))]
    pub fn new(n: u32) -> Result<Arc<Self>, AllocError> {
        Ok(Arc::new(WaitGroup {
            remaining: AtomicU32::new(n),
            waker: WakerSlot::new(),
        }))
    }

    #[cfg(kernel)]
    pub fn new(n: u32) -> Result<Arc<Self>> {
        Ok(Arc::new(
            WaitGroup {
                remaining: AtomicU32::new(n),
                waker: WakerSlot::new(),
            },
            GFP_KERNEL,
        )?)
    }

    /// Signal that one task has finished. The last one wakes the waiter.
    pub fn done(&self) {
        if self.remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
            if let Some(waker) = self.waker.take() {
                waker.wake();
            }
        }
    }

    /// A future that resolves once every expected [`done`](WaitGroup::done) has
    /// been called.
    pub fn wait(self: &Arc<Self>) -> WaitGroupWait {
        WaitGroupWait { wg: self.clone() }
    }
}

/// The future returned by [`WaitGroup::wait`].
pub struct WaitGroupWait {
    wg: Arc<WaitGroup>,
}

impl Future for WaitGroupWait {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        // Register before the count check so a `done` landing in between can't be
        // lost: either we observe zero here, or `done` finds our waker and the
        // re-poll observes it.
        self.wg.waker.register(cx.waker().clone());
        if self.wg.remaining.load(Ordering::Acquire) == 0 {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

/// A single waker slot under a tiny spinlock: pure core, the same in both
/// builds. The critical section only moves an Option<Waker>, and the only
/// contenders are the waiter and the waker.
struct WakerSlot {
    locked: AtomicBool,
    waker: UnsafeCell<Option<Waker>>,
}

// SAFETY: every access to `waker` happens under the `locked` spinlock.
unsafe impl Sync for WakerSlot {}

impl WakerSlot {
    fn new() -> Self {
        WakerSlot {
            locked: AtomicBool::new(false),
            waker: UnsafeCell::new(None),
        }
    }

    fn guard(&self) -> WakerGuard<'_> {
        while self.locked.swap(true, Ordering::Acquire) {
            core::hint::spin_loop();
        }
        WakerGuard(self)
    }

    fn register(&self, waker: Waker) {
        let g = self.guard();
        // SAFETY: held under the spinlock.
        unsafe { *g.0.waker.get() = Some(waker); }
    }

    fn take(&self) -> Option<Waker> {
        let g = self.guard();
        // SAFETY: held under the spinlock.
        unsafe { (*g.0.waker.get()).take() }
    }
}

struct WakerGuard<'a>(&'a WakerSlot);

impl Drop for WakerGuard<'_> {
    fn drop(&mut self) {
        self.0.locked.store(false, Ordering::Release);
    }
}

// ---- block_on: drive a future to completion from a synchronous caller ----

/// Drive `future` to completion on the calling thread, parking when it is pending
/// and re-polling on each wake.
#[cfg(not(kernel))]
pub fn block_on<F: Future>(future: F) -> F::Output {
    use std::task::Wake;

    struct Parker {
        signalled: Mutex<bool>,
        cond: Condvar,
    }

    impl Parker {
        fn park(&self) {
            let mut signalled = self.signalled.lock().unwrap();
            while !*signalled {
                signalled = self.cond.wait(signalled).unwrap();
            }
            *signalled = false;
        }
    }

    impl Wake for Parker {
        fn wake(self: Arc<Self>) {
            *self.signalled.lock().unwrap() = true;
            self.cond.notify_one();
        }
    }

    let parker = Arc::new(Parker {
        signalled: Mutex::new(false),
        cond: Condvar::new(),
    });
    let waker: Waker = parker.clone().into();
    let mut cx = Context::from_waker(&waker);

    let mut future = core::pin::pin!(future);
    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(value) => return value,
            Poll::Pending => parker.park(),
        }
    }
}

// A guarded flag and a CondVar, both pin-init, so the Parker is pinned in an
// Arc; woken through a hand-built RawWaker, the kernel having no Wake trait.
// The Arc keeps the waker sound if the future keeps a clone.
#[cfg(kernel)]
pub fn block_on<F: Future>(future: F) -> F::Output {
    use kernel::sync::{new_condvar, new_mutex, CondVar, Mutex};

    #[pin_data]
    struct Parker {
        #[pin]
        signalled: Mutex<bool>,
        #[pin]
        cond: CondVar,
    }

    impl Parker {
        fn park(&self) {
            let mut signalled = self.signalled.lock();
            while !*signalled {
                self.cond.wait(&mut signalled);
            }
            *signalled = false;
        }

        fn unpark(&self) {
            let mut signalled = self.signalled.lock();
            *signalled = true;
            self.cond.notify_one();
        }
    }

    // RawWaker over `Arc<Parker>`, mirroring the Task waker above; the kernel
    // `Arc` round-trips through `into_raw`/`from_raw`.
    fn raw(parker: Arc<Parker>) -> RawWaker {
        RawWaker::new(parker.into_raw().cast(), &VTABLE)
    }
    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop_waker);
    unsafe fn clone(p: *const ()) -> RawWaker {
        let parker = unsafe { Arc::from_raw(p.cast::<Parker>()) };
        let cloned = parker.clone();
        core::mem::forget(parker); // keep the ref the raw waker owns
        raw(cloned)
    }
    unsafe fn wake(p: *const ()) {
        unsafe { Arc::from_raw(p.cast::<Parker>()) }.unpark();
    }
    unsafe fn wake_by_ref(p: *const ()) {
        let parker = unsafe { Arc::from_raw(p.cast::<Parker>()) };
        parker.unpark();
        core::mem::forget(parker);
    }
    unsafe fn drop_waker(p: *const ()) {
        drop(unsafe { Arc::from_raw(p.cast::<Parker>()) });
    }

    // A fixed, tiny allocation; a failure here means OOM during perf-test setup,
    // so panic rather than thread an error through the cfg-free signature.
    let parker: Arc<Parker> = Arc::pin_init(
        pin_init!(Parker {
            signalled <- new_mutex!(false),
            cond <- new_condvar!(),
        }),
        GFP_KERNEL,
    )
    .expect("block_on: parker allocation");

    let waker = unsafe { Waker::from_raw(raw(parker.clone())) };
    let mut cx = Context::from_waker(&waker);

    let mut future = core::pin::pin!(future);
    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(value) => return value,
            Poll::Pending => parker.park(),
        }
    }
}

// ---- Waker bridge: Arc<Task> <-> core::task::Waker ----
// Both builds' Arc::into_raw()/from_raw() own as std's do; the kernel
// workqueue round-trips its Arc the same way.

fn waker_for<F: Future<Output = ()> + Send + 'static>(task: Arc<Task<F>>) -> Waker {
    // SAFETY: the vtable upholds the Waker contract.
    unsafe { Waker::from_raw(raw_waker(task)) }
}

fn raw_waker<F: Future<Output = ()> + Send + 'static>(task: Arc<Task<F>>) -> RawWaker {
    RawWaker::new(Arc::into_raw(task).cast(), vtable::<F>())
}

fn vtable<F: Future<Output = ()> + Send + 'static>() -> &'static RawWakerVTable {
    // `RawWakerVTable::new` is const, so `&{ ... }` const-promotes to 'static.
    &RawWakerVTable::new(clone::<F>, wake_owned::<F>, wake_ref::<F>, drop_ref::<F>)
}

unsafe fn clone<F: Future<Output = ()> + Send + 'static>(p: *const ()) -> RawWaker {
    let task = unsafe { Arc::from_raw(p.cast::<Task<F>>()) };
    let cloned = task.clone();
    core::mem::forget(task); // keep the ref the raw waker owns
    raw_waker(cloned)
}

unsafe fn wake_owned<F: Future<Output = ()> + Send + 'static>(p: *const ()) {
    let task = unsafe { Arc::from_raw(p.cast::<Task<F>>()) };
    task.wake();
}

unsafe fn wake_ref<F: Future<Output = ()> + Send + 'static>(p: *const ()) {
    let task = unsafe { Arc::from_raw(p.cast::<Task<F>>()) };
    let borrowed = task.clone();
    core::mem::forget(task);
    borrowed.wake();
}

unsafe fn drop_ref<F: Future<Output = ()> + Send + 'static>(p: *const ()) {
    drop(unsafe { Arc::from_raw(p.cast::<Task<F>>()) });
}
