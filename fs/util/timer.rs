// SPDX-License-Identifier: GPL-2.0

//! A delay's completion: its input - a DynDone, a Select's - completed once
//! the delay's passed, by a timer; or as it's dropped, if that's first, the
//! timer deleted. Either way exactly once, and before the drop returns.
//!
//! with_timeout() is a WaitList wait with one: a Select of the wait's
//! completion and a Timer. A wake that took the wait before its cancel is
//! waited for, to the Select's bit for it - the wake's last touch of the
//! wait.

use core::cell::UnsafeCell;
use core::convert::Infallible;
use core::mem::offset_of;
use core::pin::{pin, Pin};
use core::ptr::NonNull;
use crate::new_timer;
use crate::os::kernel::jiffies;
use crate::os::timer::{run_raw, RawTimer, TimerList};
use crate::util::sleep::SleepError;
use crate::util::task::{waiter, DynComplete, DynDone, Resumable, Resume, Select, WaitList,
    WaitlistCompletion};
use pin_init::{pin_data, pin_init, pinned_drop, stack_pin_init, PinInit};

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
    pub fn new(done: DynDone<'a>, delay: usize) -> impl PinInit<Self> {
        pin_init!(Self {
            timer <- new_timer!(run_raw::<Self>, "bcachefs timer"),
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

/// Waits on @list, under @key, for a wake - or for @ready(), checked once
/// it's on, as WaitlistCompletion::new() does - for at most @timeout jiffies:
/// Err(Timeout) if the timeout came first.
pub async fn with_timeout<K>(timeout: usize, at: Resume<'_>, list: &WaitList<K>, key: K,
                             ready: impl FnOnce() -> bool) -> Result<(), SleepError>
{
    const DONE: u32 = 1 << 0;

    let mut select = pin!(None);
    let (mut first, [done, timer_done]) = Select::new(select.as_mut(), waiter::Async::current().await);
    let mut slot = pin!(None);
    let wait = WaitlistCompletion::new(slot.as_mut(), list, key, DynComplete::new(done), ready);

    let mut ended = {
        stack_pin_init!(let _timer = Timer::new(timer_done, timeout));
        first.next().resume_on(at).await
    };
    let woken = ended & DONE != 0 || !wait.cancel();
    while ended & DONE == 0 {
        ended |= first.next().resume_on(at).await;
    }
    if woken { Ok(()) } else { Err(SleepError::Timeout) }
}
