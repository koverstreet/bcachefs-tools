// SPDX-License-Identifier: GPL-2.0

//! Timers, as the kernel's: what's pending, by when, in a list under one
//! lock, each run when it's due by a thread of their own - which records the
//! one it's running, for a delete to wait out. Nothing in C has one: the
//! struct is Rust's.

use crate::os::kernel::{jiffies, jiffies_to_msecs, time_after_eq};
use crate::util::ffi::c_ulong;
use core::marker::PhantomPinned;
use core::ptr::NonNull;
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::Duration;

/// Deleted as it drops, a run under way waited out: the list has it by
/// address only while it's pending.
pub struct TimerList {
    func: TimerFn,
    _pin: PhantomPinned,
}

pub type TimerFn = unsafe fn(*mut TimerList);

/// A TimerList at a fixed place in Self, running Self::run(): new_timer!()
/// with run_raw::<Self>, the trampoline, which run() is inlined into.
pub trait RawTimer {
    /// offset_of!() the TimerList
    const TIMER: usize;

    /// # Safety
    /// @this's timer expired.
    unsafe fn run(this: NonNull<Self>);
}

#[doc(hidden)]
pub unsafe fn run_raw<T: RawTimer>(timer: *mut TimerList) {
    unsafe { T::run(NonNull::new_unchecked(timer.byte_sub(T::TIMER).cast())) }
}

struct Pending {
    at:    c_ulong,
    timer: NonNull<TimerList>,
}

struct State {
    pending: Vec<Pending>,
    running: Option<NonNull<TimerList>>,
    thread:  bool,
}

// what it points to is pending, and deleted before it's freed
unsafe impl Send for State {}

static LOCK: Mutex<State> = Mutex::new(State { pending: Vec::new(), running: None, thread: false });
static ADDED: Condvar = Condvar::new();
static RAN: Condvar = Condvar::new();

fn lock() -> MutexGuard<'static, State> {
    LOCK.lock().expect("timer lock poisoned")
}

fn timer_thread() {
    let mut st = lock();
    loop {
        let now = jiffies();
        if let Some(i) = st.pending.iter().position(|p| time_after_eq(now, p.at)) {
            let t = st.pending.swap_remove(i).timer;
            st.running = Some(t);
            drop(st);
            unsafe { (t.as_ref().func)(t.as_ptr()) };
            st = lock();
            st.running = None;
            RAN.notify_all();
            continue;
        }

        st = match st.pending.iter().map(|p| p.at.wrapping_sub(now)).min() {
            None    => ADDED.wait(st).expect("timer lock poisoned"),
            Some(j) => ADDED.wait_timeout(st, Duration::from_millis(jiffies_to_msecs(j).into()))
                .expect("timer lock poisoned").0,
        };
    }
}

impl TimerList {
    #[doc(hidden)]
    pub fn new(func: TimerFn) -> impl ::pin_init::PinInit<Self> {
        unsafe {
            ::pin_init::pin_init_from_closure(move |slot: *mut Self| {
                slot.write(Self { func, _pin: PhantomPinned });
                Ok(())
            })
        }
    }

    fn remove(&self, st: &mut State) -> bool {
        let me = NonNull::from(self);
        st.pending.iter().position(|p| p.timer == me)
            .map(|i| st.pending.swap_remove(i))
            .is_some()
    }

    /// Run at @expires, in jiffies - moved there if it was pending:
    /// mod_timer(). Whether it was.
    pub fn modify(&self, expires: c_ulong) -> bool {
        let mut st = lock();
        let was = self.remove(&mut st);
        st.pending.push(Pending { at: expires, timer: NonNull::from(self) });
        if !st.thread {
            std::thread::Builder::new()
                .name("timers".into())
                .spawn(timer_thread)
                .expect("error creating the timer thread");
            st.thread = true;
        }
        ADDED.notify_one();
        was
    }

    /// Not pending any more, and not running - waiting out a run that is:
    /// timer_delete_sync(). Whether it was pending.
    pub fn delete_sync(&self) -> bool {
        let mut st = lock();
        while st.running == Some(NonNull::from(self)) {
            st = RAN.wait(st).expect("timer lock poisoned");
        }
        self.remove(&mut st)
    }
}

impl Drop for TimerList {
    fn drop(&mut self) {
        self.delete_sync();
    }
}

/// A TimerList's initializer, running @func: timer_setup(). @name is the
/// kernel port's, for its lockdep class.
#[macro_export]
macro_rules! new_timer {
    ($func:expr, $name:literal $(,)?) => {
        $crate::os::timer::TimerList::new($func)
    };
}
