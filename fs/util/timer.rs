// SPDX-License-Identifier: GPL-2.0

//! A delay's completion: its input - a DynDone, a Select's - completed once
//! the delay's passed, by a timer; or as it's dropped, if that's first, the
//! timer deleted. Either way exactly once, and before the drop returns.

use crate::os::kernel::jiffies;
use crate::os::timer::{run_raw, RawTimer, TimerList};
use crate::util::task::DynDone;
use core::cell::UnsafeCell;
use core::convert::Infallible;
use core::ffi::c_ulong;
use core::mem::offset_of;
use core::pin::Pin;
use core::ptr::NonNull;
use pin_init::{pin_data, pin_init, pinned_drop, PinInit};

#[pin_data(PinnedDrop)]
pub struct Timer<'a> {
    #[pin]
    timer: TimerList,
    /* taken by the timer, or by the drop that deletes it */
    done:  UnsafeCell<Option<DynDone<'a>>>,
}

// its Done is taken by the timer, or after delete_sync() has stopped it
unsafe impl Sync for Timer<'_> {}

impl<'a> Timer<'a> {
    /// @done completed in @delay jiffies.
    pub fn new(done: DynDone<'a>, delay: c_ulong) -> impl PinInit<Self> {
        pin_init!(Self {
            timer <- crate::new_timer!(run_raw::<Self>, "bcachefs timer"),
            done:  UnsafeCell::new(Some(done)),
        })
        .pin_chain(move |t| {
            t.timer.modify(jiffies().wrapping_add(delay));
            Ok::<(), Infallible>(())
        })
    }
}

impl RawTimer for Timer<'_> {
    const TIMER: usize = offset_of!(Self, timer);

    unsafe fn run(t: NonNull<Self>) {
        unsafe { (*t.as_ref().done.get()).take() }.expect("a timer run twice").complete();
    }
}

#[pinned_drop]
impl PinnedDrop for Timer<'_> {
    fn drop(self: Pin<&mut Self>) {
        // still pending, it never ran: its completion is ours
        if self.timer.delete_sync() {
            unsafe { (*self.done.get()).take() }.expect("a deleted timer, completed").complete();
        }
    }
}
