// SPDX-License-Identifier: GPL-2.0

//! A RunQueue's tasks run only in its owner's run(): a mover's updates, run
//! between its own steps instead of competing with it. A dispatch pushes the
//! task on a lockless stack - from any context, an endio's - and wakes the
//! queue's EventCount; run() takes the stack whole and runs it oldest first,
//! in the order the tasks were woken.
//!
//! Its owner is a thread, waiting on the EventCount - or its own work item, a
//! worker of its own while it has tasks and nothing when it's idle: a write
//! point's index updates. An await resumed on a worked RunQueue - with
//! resume_on_run_queue() - is dispatched onto it and queues the work, which
//! run()s it, on the RunQueue's queue: always the same one, as a work item is
//! non-reentrant only on one.

use super::{Header, TaskRef};
use crate::c;
use crate::os::workqueue::{run_raw, Queue, RawWork};
use crate::util::futex::EventCount;
use crate::util::late::LateInit;
use core::marker::PhantomPinned;
use core::pin::Pin;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicPtr, Ordering};
use cstruct_macros::MaybeZeroable;
use pin_init::{pin_data, pin_init, pinned_drop, PinInit};

/// Pinned: its tasks hold its address. Its owner joins them all before it's
/// gone.
#[pin_data(PinnedDrop)]
#[derive(MaybeZeroable)]
pub struct RunQueue {
    /// Newest first, through runq_next.
    head:    AtomicPtr<Header>,
    events:  EventCount,
    /// A worked one's: zeroed, and never queued, for a thread's
    #[pin]
    work:    c::work_struct,
    /// A worked one's: where its work is queued
    queue:   Option<NonNull<Queue>>,
    #[pin]
    _pinned: PhantomPinned,
}

// pushed to from any context; its work is only queued, by the workqueue's
// rules, never touched in place
unsafe impl Send for RunQueue {}
unsafe impl Sync for RunQueue {}

impl RunQueue {
    /// One run by a work item of its own, queued on @q.
    ///
    /// # Safety
    /// @q outlives every dispatch onto it.
    pub unsafe fn worked(q: &Queue) -> impl PinInit<Self> {
        let queue = Some(NonNull::from(q));
        pin_init!(RunQueue {
            head:    AtomicPtr::new(core::ptr::null_mut()),
            events:  EventCount::new(),
            work     <- crate::new_work!(run_raw::<RunQueue>, "bcachefs run queue"),
            queue,
            _pinned: PhantomPinned,
        })
    }

    /// A dispatch onto a worked() one: pushed, and its work queued.
    ///
    /// # Safety
    /// As push()'s.
    pub(super) unsafe fn push_worked(&self, h: NonNull<Header>) {
        let q = self.queue.expect("an await resumed on a RunQueue with no work of its own");
        unsafe {
            self.push(h);
            q.as_ref().queue_work(core::ptr::from_ref(&self.work).cast_mut());
        }
    }

    /// Woken by every dispatch, and whatever else the owner wakes it with.
    pub fn events(&self) -> &EventCount {
        &self.events
    }

    pub fn has_ready(&self) -> bool {
        !self.head.load(Ordering::Acquire).is_null()
    }

    /// # Safety
    /// @h is a live task's, owned and dispatched here.
    pub(super) unsafe fn push(&self, h: NonNull<Header>) {
        let hr = unsafe { h.as_ref() };
        let mut head = self.head.load(Ordering::Relaxed);
        loop {
            hr.runq_next.store(head, Ordering::Relaxed);
            match self.head.compare_exchange_weak(head, h.as_ptr(), Ordering::Release, Ordering::Relaxed) {
                Ok(_)    => break,
                Err(cur) => head = cur,
            }
        }
        self.events.wake();
    }

    /// Only one thread may run it. What's dispatched meanwhile waits for the
    /// next run().
    pub fn run(self: Pin<&Self>) {
        let mut next = self.head.swap(core::ptr::null_mut(), Ordering::Acquire);

        // pushed newest first: reversed, for the order they were woken
        let mut oldest = core::ptr::null_mut::<Header>();
        while let Some(h) = NonNull::new(next) {
            // SAFETY: dispatched here: ours to run
            let hr = unsafe { h.as_ref() };
            next = hr.runq_next.load(Ordering::Relaxed);
            hr.runq_next.store(oldest, Ordering::Relaxed);
            oldest = h.as_ptr();
        }

        while let Some(h) = NonNull::new(oldest) {
            // read before it's run, and maybe freed
            oldest = unsafe { h.as_ref() }.runq_next.load(Ordering::Relaxed);
            unsafe { TaskRef(h).run(h.as_ref().target.get()) };
        }
    }
}

/* a worked one's, made late: once it has its queue */
impl LateInit for RunQueue {
    fn is_made(&self) -> bool {
        self.queue.is_some()
    }
}

impl RawWork for RunQueue {
    const WORK: usize = core::mem::offset_of!(RunQueue, work);

    unsafe fn run(rq: NonNull<Self>) {
        unsafe { Pin::new_unchecked(rq.as_ref()) }.run()
    }
}

#[pinned_drop]
impl PinnedDrop for RunQueue {
    fn drop(self: Pin<&mut Self>) {
        assert!(!self.has_ready(), "RunQueue dropped with tasks dispatched to it");
    }
}
