// SPDX-License-Identifier: GPL-2.0

//! sleep_until(): the loop every wait is - set the task state, check the
//! condition, schedule() - with the condition a closure, and no wait queue.
//! Whatever makes the condition true wakes the thread itself.
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
use core::marker::PhantomData;

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
    state:   u32,
    io:      bool,
    timeout: isize,
    _e:      PhantomData<E>,
}

/// MAX_SCHEDULE_TIMEOUT: no timeout
const FOREVER: isize = isize::MAX;

impl<E> Sleep<E> {
    const fn state(state: u32) -> Self {
        Sleep { state: state as u32, io: false, timeout: FOREVER, _e: PhantomData }
    }

    pub const fn freezable(self) -> Self {
        Sleep { state: self.state | c::TASK_FREEZABLE as u32, ..self }
    }

    /// io_schedule(): counted as iowait, as wait_on_bit_io() is.
    pub const fn io(self) -> Self {
        Sleep { io: true, ..self }
    }

    pub const fn timeout(self, jiffies: usize) -> Sleep<SleepError> {
        let timeout = if jiffies >= FOREVER as usize { FOREVER - 1 } else { jiffies as isize };
        Sleep { state: self.state, io: self.io, timeout, _e: PhantomData }
    }

    /// What's left of this sleep's timeout, started at @start: for a wait
    /// that sleeps more than once.
    pub fn since(self, start: usize) -> Self {
        if self.timeout == FOREVER {
            return self;
        }
        let left = self.timeout - crate::os::kernel::jiffies().wrapping_sub(start) as isize;
        Sleep { timeout: left.max(0), ..self }
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
/// Copy, so a Sleep is.
pub trait SleepEnd: sealed::Sealed + Copy {
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
    crate::os::sched::might_sleep();

    let mut left = how.timeout;
    let ret = loop {
        // SAFETY: our own state; set before the check
        crate::os::sched::set_current_state(how.state);
        if let Some(v) = cond() {
            break Ok(v);
        }
        if left == 0 {
            break Err(E::early(SleepError::Timeout));
        }
        if crate::os::sched::signal_pending_state(how.state) {
            break Err(E::early(SleepError::Signal));
        }
        left = if how.io {
            crate::os::sched::io_schedule_timeout(left)
        } else {
            crate::os::sched::schedule_timeout(left)
        };
    };
    crate::os::sched::set_current_state_running();
    ret
}

