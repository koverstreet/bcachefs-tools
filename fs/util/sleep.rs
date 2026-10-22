// SPDX-License-Identifier: GPL-2.0

//! sleep_until(): the loop every wait is - set the task state, check the
//! condition, schedule() - with the condition a closure, and no wait queue.
//! Whatever makes the condition true wakes the task itself: through a Waker
//! it was handed - task_waker(), below - or a futex_wake().
//!
//! The state is set before the check, so a wake landing between the check
//! and schedule() makes schedule() return at once: no wakeup is lost. The
//! condition runs in that state, not TASK_RUNNING, so it mustn't sleep.
//!
//! How a sleep may end is in its type: Sleep<Infallible> - uninterruptible,
//! no timeout - only ends with the condition true, and sleep_until() says so,
//! as Result<T, Infallible>: `let Ok(v) = sleep_until(..)`. Interruptible,
//! killable and timed sleeps are Sleep<SleepError>.
//!
//! A timeout is in jiffies, for the whole sleep, not each pass: what
//! schedule_timeout() returns, the time left, is the next pass's.

use crate::c;
use core::convert::Infallible;
use core::ffi::{c_long, c_uint, c_ulong};
use core::marker::PhantomData;
use core::task::{RawWaker, RawWakerVTable, Waker};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SleepError {
    /// A signal, for an interruptible sleep - a fatal one, for a killable
    Signal,
    Timeout,
}

/// How to sleep: the task state, and whether it's counted as waiting on IO.
/// E is what ending without the condition true is: Infallible when it can't.
#[derive(Clone, Copy)]
pub struct Sleep<E = Infallible> {
    state:   c_uint,
    io:      bool,
    timeout: c_long,
    _e:      PhantomData<E>,
}

/// MAX_SCHEDULE_TIMEOUT: no timeout
const FOREVER: c_long = c_long::MAX;

impl<E> Sleep<E> {
    const fn state(state: u32) -> Self {
        Sleep { state: state as c_uint, io: false, timeout: FOREVER, _e: PhantomData }
    }

    pub const fn freezable(self) -> Self {
        Sleep { state: self.state | c::TASK_FREEZABLE as c_uint, ..self }
    }

    /// io_schedule(): counted as iowait, as wait_on_bit_io() is.
    pub const fn io(self) -> Self {
        Sleep { io: true, ..self }
    }

    pub const fn timeout(self, jiffies: c_ulong) -> Sleep<SleepError> {
        let timeout = if jiffies >= FOREVER as c_ulong { FOREVER - 1 } else { jiffies as c_long };
        Sleep { state: self.state, io: self.io, timeout, _e: PhantomData }
    }
}

impl Sleep<Infallible> {
    pub const UNINTERRUPTIBLE: Self = Self::state(c::TASK_UNINTERRUPTIBLE);
    /// Uninterruptible, without counting toward the load average or hung
    /// task warnings: a wait that may be long and is expected to be.
    pub const IDLE: Self = Self::state(c::TASK_IDLE);
}

impl Sleep<SleepError> {
    pub const INTERRUPTIBLE: Self = Self::state(c::TASK_INTERRUPTIBLE);
    pub const KILLABLE: Self = Self::state(c::TASK_KILLABLE);
}

/// SleepError, or Infallible: a sleep that can't end early never makes one.
pub trait SleepEnd: sealed::Sealed {
    fn early(e: SleepError) -> Self;
}

mod sealed {
    pub trait Sealed {}
    impl Sealed for super::SleepError {}
    impl Sealed for core::convert::Infallible {}
}

impl SleepEnd for SleepError {
    fn early(e: SleepError) -> Self {
        e
    }
}

impl SleepEnd for Infallible {
    fn early(e: SleepError) -> Self {
        unreachable!("an uninterruptible sleep with no timeout ended early: {e:?}")
    }
}

/// Sleep, as @how says, until @cond returns Some - which it may at once.
pub fn sleep_until<T, E: SleepEnd>(how: Sleep<E>, mut cond: impl FnMut() -> Option<T>) -> Result<T, E> {
    #[cfg(kernel)]
    ::kernel::task::might_sleep();

    let mut left = how.timeout;
    let ret = loop {
        // SAFETY: our own state; set before the check
        unsafe { c::bch2_set_current_state(how.state) };
        if let Some(v) = cond() {
            break Ok(v);
        }
        if left == 0 {
            break Err(E::early(SleepError::Timeout));
        }
        if unsafe { c::bch2_signal_pending_state(how.state) } {
            break Err(E::early(SleepError::Signal));
        }
        // SAFETY: in a sleeping state we set; a timeout of FOREVER is
        // returned as is. `as _`: long is isize in the kernel's bindings,
        // i64 in ours.
        left = unsafe {
            if how.io { c::io_schedule_timeout(left as _) } else { c::schedule_timeout(left as _) }
        } as c_long;
    };
    unsafe { c::bch2_set_current_state_running() };
    ret
}

/// A Waker for a blocking wait: wakes this task - current - and holds a ref
/// on it, so whatever wakes it can do so after the waiter may have returned.
pub fn task_waker() -> Waker {
    // SAFETY: a ref on ourself, the Waker's
    unsafe {
        let task = c::bch2_current();
        assert!(!task.is_null(), "a blocking wait needs a task - a kthread, in userspace");
        c::get_task_struct(task);
        Waker::from_raw(RawWaker::new(task.cast(), &TASK_WAKER))
    }
}

static TASK_WAKER: RawWakerVTable = RawWakerVTable::new(task_clone, task_wake, task_wake_by_ref, task_drop);

unsafe fn task_clone(p: *const ()) -> RawWaker {
    // SAFETY: a task_waker()'s task, a ref held
    unsafe { c::get_task_struct(p.cast_mut().cast()) };
    RawWaker::new(p, &TASK_WAKER)
}

unsafe fn task_wake(p: *const ()) {
    // SAFETY: an owned Waker: woken, then its ref put
    unsafe {
        task_wake_by_ref(p);
        task_drop(p);
    }
}

unsafe fn task_wake_by_ref(p: *const ()) {
    // SAFETY: a ref held
    unsafe { c::wake_up_process(p.cast_mut().cast()) };
}

unsafe fn task_drop(p: *const ()) {
    // SAFETY: the Waker's ref
    unsafe { c::put_task_struct(p.cast_mut().cast()) };
}
