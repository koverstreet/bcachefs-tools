// SPDX-License-Identifier: GPL-2.0

//! A RunQueue's tasks run only in its owner's run(): a mover's updates, run
//! between its own steps instead of competing with it. A dispatch pushes the
//! task on a lockless stack - from any context, an endio's - and wakes the
//! queue's EventCount; run() takes the stack whole and runs it oldest first,
//! in the order the tasks were woken.

use super::{Header, TaskRef};
use crate::util::futex::EventCount;
use core::marker::PhantomPinned;
use core::pin::Pin;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicPtr, Ordering};

/// Pinned: its tasks hold its address. Its owner joins them all before it's
/// gone.
pub struct RunQueue {
    /// Newest first, through runq_next.
    head:    AtomicPtr<Header>,
    events:  EventCount,
    _pinned: PhantomPinned,
}

// SAFETY: zeroed is empty
unsafe impl ::pin_init::Zeroable for RunQueue {}

impl RunQueue {
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
            unsafe { TaskRef(h).run() };
        }
    }
}

impl Drop for RunQueue {
    fn drop(&mut self) {
        assert!(!self.has_ready(), "RunQueue dropped with tasks dispatched to it");
    }
}
