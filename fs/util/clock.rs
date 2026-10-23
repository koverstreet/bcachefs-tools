// SPDX-License-Identifier: GPL-2.0

//! Waiting on an io clock - util/clock.c's clocks of sectors of IO done: a
//! timer on the clock that wakes the waiter when the clock reaches its
//! time, and sleep_until() on that or the waiter's own condition, as
//! bch2_kthread_io_clock_wait_once() and its caller's recheck are in C.
//!
//! The timer's on the waiter's stack, in the clock's heap while it waits: a
//! guard takes it out on the way out, however that is. It fires from the IO
//! path - __bch2_increment_clock(), any context - so all it does is set a
//! flag and wake the task; and it fires under the clock's timer_lock, which
//! taking it out takes too, so once it's out it isn't firing.

use crate::c;
use crate::util::sleep::{sleep_until, task_waker, Sleep, SleepEnd};
use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, Ordering};
use core::task::Waker;

/// A waiter's timer: io_timer first, so the timer's address is its.
#[repr(C)]
struct Wait {
    timer:   c::io_timer,
    expired: AtomicBool,
    waker:   Waker,
}

unsafe extern "C" fn wait_expired(timer: *mut c::io_timer) {
    // SAFETY: a Wait's timer, alive while it's on the clock - and it's
    // taken off before the Wait goes
    let w = unsafe { &*timer.cast::<Wait>() };
    w.expired.store(true, Ordering::Release);
    w.waker.wake_by_ref();
}

/// Takes the timer off the clock, if it hasn't fired.
struct OnClock<'a>(&'a c::io_clock, &'a Wait);

impl Drop for OnClock<'_> {
    fn drop(&mut self) {
        // SAFETY: the clock's, changed by C under its timer_lock
        unsafe { c::bch2_io_timer_del(core::ptr::from_ref(self.0).cast_mut(),
                                      core::ptr::from_ref(&self.1.timer).cast_mut()) };
    }
}

impl c::io_clock {
    /// Sleep, as @how says, until the clock reaches @until - None - or
    /// @cond() says otherwise, checked after every wake: kthread_should_stop()
    /// say. @cond runs in the sleep's task state, so it mustn't sleep.
    pub fn wait_until<T, E: SleepEnd>(&self, until: u64, how: Sleep<E>,
                                      mut cond: impl FnMut() -> Option<T>) -> Result<Option<T>, E> {
        let w = Wait {
            timer:   c::io_timer { fn_: Some(wait_expired), fn2: wait_expired as *mut c_void, expire: until },
            expired: AtomicBool::new(false),
            waker:   task_waker(),
        };
        // SAFETY: as OnClock's: w stays put, and off the clock before it goes
        unsafe { c::bch2_io_timer_add(core::ptr::from_ref(self).cast_mut(),
                                      core::ptr::from_ref(&w.timer).cast_mut()) };
        let _on_clock = OnClock(self, &w);

        sleep_until(how, || match cond() {
            Some(v) => Some(Some(v)),
            None    => w.expired.load(Ordering::Acquire).then_some(None),
        })
    }
}
