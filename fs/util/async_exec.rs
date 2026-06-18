// SPDX-License-Identifier: GPL-2.0
//! An async executor backed by a kernel workqueue.
//!
//! Modernization of Wedson Almeida Filho's `kasync` workqueue executor
//! (out-of-tree rust-for-linux), onto the current in-kernel `kernel::workqueue`
//! and the userspace shim's matching `Work`/`WorkItem`/`enqueue` API.
//!
//! Model: a task is a future plus an embedded `work_struct`. The workqueue runs
//! the task (`WorkItem::run`), which polls the future once; the future's waker
//! re-`enqueue`s the task to be polled again. The `work_struct`'s pending bit
//! means a task is never run — and so never polled — concurrently with itself,
//! which gives the "polled by one thread at a time" guarantee for free (so,
//! unlike a per-wake-`spawn` design, no hand-rolled state machine).
//!
//! Not yet here: a join handle (await a task's result) and a `block_on` for sync
//! callers. Those are the integration layer the perf test will need.

use core::cell::UnsafeCell;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

#[cfg(kernel)]
use kernel::{
    prelude::*,
    sync::Arc,
    workqueue::{Queue, Work, WorkItem},
};

#[cfg(not(kernel))]
use bcachefs_shim::workqueue::{Queue, Work, WorkItem};
#[cfg(not(kernel))]
use std::sync::Arc;

#[cfg_attr(kernel, pin_data)]
struct Task<F: Future<Output = ()> + Send + 'static> {
    #[cfg_attr(kernel, pin)]
    work: Work<Task<F>>,
    queue: &'static Queue,
    future: UnsafeCell<F>,
}

// SAFETY: the work_struct's pending bit means `run` (the only place the future
// is touched) executes on one thread at a time, so the future is never polled
// concurrently; the task is `Sync` whenever the future is `Send`.
unsafe impl<F: Future<Output = ()> + Send + 'static> Sync for Task<F> {}

impl<F: Future<Output = ()> + Send + 'static> Task<F> {
    /// Poll the future once, on a workqueue thread.
    fn poll(self: Arc<Self>) {
        let waker = waker_for(self.clone());
        let mut cx = Context::from_waker(&waker);

        // SAFETY: the work_struct serializes runs, so access is exclusive here,
        // and the future never moves (it lives behind `Arc`).
        let future = unsafe { Pin::new_unchecked(&mut *self.future.get()) };

        // Ready: nothing re-enqueues us, the Arc refs drain, the task frees.
        // Pending: the future kept the waker; waking it re-enqueues a poll.
        let _: Poll<()> = future.poll(&mut cx);
    }

    /// Re-run this task on its workqueue (the waker calls this).
    fn wake(self: Arc<Self>) {
        let queue = self.queue;
        queue.enqueue(self);
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

// FLAG(kernel, verify): in-kernel the `Work<T>` offset is wired via the
// `impl_has_work!` macro; confirm the generic syntax against this tree's
// kernel crate version.
#[cfg(kernel)]
kernel::impl_has_work! {
    impl{F: Future<Output = ()> + Send + 'static} HasWork<Self> for Task<F> { self.work }
}

/// Spawn `future` onto `queue`. Fire-and-forget for now (no join handle yet).
#[cfg(not(kernel))]
pub fn spawn<F: Future<Output = ()> + Send + 'static>(queue: &'static Queue, future: F) {
    let task = Arc::new(Task {
        work: Work::new(),
        queue,
        future: UnsafeCell::new(future),
    });
    queue.enqueue(task);
}

// FLAG(kernel, verify): the kernel half. `Work` is `#[pin]`, so construction is
// pin-init; `Work::new()`'s real args (name + lock-class key) and the
// `Arc::pin_init`/`GFP`/`Result` shape need confirming against the kernel crate.
#[cfg(kernel)]
pub fn spawn<F: Future<Output = ()> + Send + 'static>(
    queue: &'static Queue,
    future: F,
) -> Result<()> {
    let task = Arc::pin_init(
        pin_init!(Task {
            work <- Work::new(),
            queue,
            future: UnsafeCell::new(future),
        }),
        GFP_KERNEL,
    )?;
    queue.enqueue(task);
    Ok(())
}

// ---- Waker bridge: Arc<Task> <-> core::task::Waker ----
// NOTE(kernel, verify): std `Arc` has `into_raw`/`from_raw` (verified on the
// userspace side); the kernel `Arc` round-trips via `ForeignOwnable`
// (`into_foreign`/`from_foreign`/`borrow`). This block likely needs a thin cfg
// or a 2-line trait to name one API across both sides.

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
