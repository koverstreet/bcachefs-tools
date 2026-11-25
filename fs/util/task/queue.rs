// SPDX-License-Identifier: GPL-2.0

//! What's dispatched on a RunQueue runs only in its owner's run(): a mover's
//! updates, run between its own steps instead of competing with it. A
//! dispatch pushes the task on a lockless stack - from any context, an
//! endio's - and wakes the queue's EventCount; run() takes the stack whole and
//! runs it oldest first, in the order the tasks were woken. A task runs there
//! only for the awaits that resume it there - Resume::run_queue() - and moves
//! on as its next await says.
//!
//! Its owner is a thread, waiting on the EventCount - or its own work item, a
//! worker of its own while it has tasks and nothing when it's idle: a write
//! point's index updates. A dispatch onto a worked RunQueue also queues the
//! work, which run()s it, on the RunQueue's queue: always the same one, as a
//! work item is non-reentrant only on one.

use super::{Header, TaskRef};
use crate::os::workqueue::{CancelledWork, Queue};
use crate::util::futex::EventCount;
use crate::util::rcu;
use core::pin::Pin;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicPtr, Ordering};
#[cfg(kernel)]
use kernel::sync::SetOnce;
#[cfg(not(kernel))]
use bcachefs_shim::sync::SetOnce;
use cstruct_macros::Init;
use pin_init::{pin_data, pinned_drop};

/// Pinned: its tasks hold its address. Its owner joins them all before it's
/// gone.
#[pin_data(PinnedDrop)]
#[derive(Init)]
pub struct RunQueue {
    /// A worked one's, never queued for a thread's
    #[pin]
    work:    CancelledWork<run_queue_work>,
    /// Newest first, through runq_next.
    head:    AtomicPtr<Header>,
    events:  EventCount,
    /// A worked one's: where its work is queued, from set_queue()
    #[init(SetOnce::new())]
    queue:   SetOnce<&'static Queue>,
    /// A dispatch is an RCU reader of it after its task's pushed, when the
    /// task may run, finish and free its owner: so it's an allocation of its
    /// own, a MaybeRcuAlloc, dropped a grace period on - its work cancelled
    /// once no dispatch can queue it - and its owner's drop stays its own.
    #[init(rcu::RcuDropWorkMember::default())]
    _rcu:    rcu::RcuDropWorkMember,
}

fn run_queue_work(rq: &RunQueue) {
    rq.run()
}

impl RunQueue {
    /// Run by a work item of its own from here on, queued on @q - before
    /// anything's dispatched on it. Its work may be queued, for nothing, once
    /// the last task's done - a dispatch queues it after pushing its task,
    /// which a run() under way may have taken already: its drop cancels it.
    ///
    /// # Safety
    /// @q outlives every dispatch onto it.
    pub unsafe fn set_queue(&self, q: &Queue) {
        let q: &'static Queue = unsafe { &*core::ptr::from_ref(q) };
        assert!(self.queue.populate(q), "a RunQueue's queue set twice");
    }

    /// # Safety
    /// As push()'s.
    pub(super) unsafe fn dispatch(&self, h: NonNull<Header>) {
        // the task, once pushed, may run, finish and free us: _rcu's
        let _rcu = rcu::read_lock();
        unsafe { self.push(h) };
        if let Some(&q) = self.queue.as_ref() {
            self.work.queue(q);
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
    unsafe fn push(&self, h: NonNull<Header>) {
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
    pub fn run(&self) {
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

#[pinned_drop]
impl PinnedDrop for RunQueue {
    fn drop(self: Pin<&mut Self>) {
        assert!(!self.has_ready(), "RunQueue dropped with tasks dispatched to it");
    }
}
