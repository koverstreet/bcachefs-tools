// SPDX-License-Identifier: GPL-2.0

//! The io clocks - c->io_clock[READ] and [WRITE] - and waiting on them. A
//! clock's time is sectors of IO done: increment() adds to a per-CPU batch,
//! and every IO_CLOCK_PCPU_SECTORS a CPU's batch goes to the clock, by
//! __bch2_increment_clock(); an IO that big goes to it at once. So it's
//! approximate: up to that many sectors a CPU behind. Clocks are only moved
//! from task context, IO submission, so the batch's add is a load and a store
//! with preemption off.
//!
//! A waiter - wait_until() - sleeps until the clock reaches its time, or its
//! own condition says otherwise. While it waits it has a Timer on the clock:
//! its time, a Waker for its task, and where it waits from, for the debug
//! output. The clock's timers are a vec, sorted latest first, under
//! timer_lock: the next to expire is the last, so expiring is a pop. There
//! are a few - copygc's and reconcile's - so an insert's shift is nothing.
//! The vec grows on demand: a waiter that finds it full makes it room with
//! the lock dropped, and if that fails, checks the clock once a second,
//! trying again each time.
//!
//! A clock moving on wakes - and drops - the timers it's passed, under the
//! lock, from IO submission. The waiter, its condition true or not, takes its
//! timer off on the way out - if it's still there - under the same lock. A
//! Timer's Waker is a clone of the waiter's own, so its drop is never the
//! task's last ref; a task waits on one clock at a time, so its task names
//! its Timer.
//!
//! now is read and added to Relaxed: the lock orders a waiter's add against
//! an increment, and the task state and the wake order its recheck after one.

use crate::c;
use crate::fs::Fs;
use crate::locking::Lock;
use crate::{new_lock, prt};
use crate::types::{rust_only, RustOnly};
use crate::util::alloc::{flags::GFP_KERNEL, AllocError, KVVec};
use crate::util::kernel::HZ;
use crate::util::ffi::Opaque;
use crate::util::percpu::{CpuGuard, PerCpu};
use crate::util::sleep::{sleep_until, task_waker, Sleep, SleepEnd, SleepError};
use crate::util::{Printbuf, ToText};
use core::ffi::c_int;
use core::panic::Location;
use core::pin::Pin;
use core::sync::atomic::{AtomicI64, Ordering::Relaxed};
use core::task::Waker;
use cstruct_macros::{c_const, c_export, CStruct, Init};
use pin_init::{pin_data, pinned_drop};

c_const! {
    /* Amount to buffer up on a percpu counter */
    #[c_int]
    pub const IO_CLOCK_PCPU_SECTORS: u32 = 128;
}

#[allow(non_camel_case_types)]
#[pin_data(PinnedDrop)]
#[repr(C)]
#[derive(CStruct, Init)]
pub struct io_clock {
    pub now: AtomicI64,
    #[c("u16 __percpu *pcpu_buf")]
    #[init(PerCpu::init())]
    pub pcpu_buf: PerCpu<u16>,

    /// The waiters, latest first: Rust's alone.
    #[pin]
    #[init(RustOnly::new(new_lock!(KVVec::new(), (), "&clock->timer_lock")))]
    timers: rust_only!(Lock<c::spinlock_t, KVVec<Timer>>),
}

/// A waiter on a clock: wakes @waker when the clock reaches @expire.
pub(crate) struct Timer {
    expire: u64,
    waker:  Waker,
    caller: &'static Location<'static>,
}

/// time_after_eq64(now, t): wrapping, where the timers' order is plain u64,
/// as C's heap's was
fn reached(now: u64, t: u64) -> bool {
    now.wrapping_sub(t) as i64 >= 0
}

impl io_clock {
    fn now(&self) -> u64 {
        self.now.load(Relaxed) as u64
    }

    /// Puts @t on the clock - unless the clock's already reached its time,
    /// which the waiter's next check sees - making room if it's full.
    fn add(&self, mut t: Timer) -> Result<(), AllocError> {
        loop {
            let mut timers = self.timers.lock();
            if reached(self.now(), t.expire) {
                return Ok(());
            }

            let i = timers.partition_point(|x| x.expire >= t.expire);
            match timers.push_within_capacity(t) {
                Ok(()) => {
                    timers[i..].rotate_right(1);
                    return Ok(());
                }
                Err(full) => t = full.0,
            }

            let cap = timers.capacity();
            drop(timers);
            self.grow(cap)?;
        }
    }

    /// Room for twice @cap timers, unless another waiter's made it since:
    /// allocated unlocked, swapped in under the lock, and the old buffer -
    /// empty - freed once the lock's dropped.
    fn grow(&self, cap: usize) -> Result<(), AllocError> {
        let mut new = KVVec::with_capacity((cap * 2).max(4), GFP_KERNEL)?;
        let mut timers = self.timers.lock();
        if timers.capacity() == cap {
            for t in timers.drain_all() {
                // no more than @cap of them
                let pushed = new.push_within_capacity(t).is_ok();
                debug_assert!(pushed);
            }
            core::mem::swap(&mut *timers, &mut new);
        }
        Ok(())
    }

    /// Takes @waker's timer off the clock, if it's still on.
    fn del(&self, waker: &Waker) {
        self.timers.lock().retain(|t| t.waker.data() != waker.data());
    }

    /// Sleep, as @how says, until the clock reaches @until - None - or
    /// @cond() says otherwise, checked after every wake: kthread_should_stop()
    /// say. @cond runs in the sleep's task state, so it mustn't sleep.
    ///
    /// With no room for a timer, and none to be had, it sleeps a second at a
    /// time, trying again after each: a timeout in @how, which no caller
    /// gives, is a second's, each time.
    #[track_caller]
    pub fn wait_until<T, E: SleepEnd>(&self, until: u64, how: Sleep<E>,
                                      mut cond: impl FnMut() -> Option<T>) -> Result<Option<T>, E> {
        let caller = Location::caller();
        let mut done = || match cond() {
            Some(v) => Some(Some(v)),
            None    => reached(self.now(), until).then_some(None),
        };

        let waker = task_waker();
        loop {
            if self.add(Timer { expire: until, waker: waker.clone(), caller }).is_ok() {
                let ret = sleep_until(how, &mut done);
                self.del(&waker);
                return ret;
            }

            match sleep_until(how.timeout(HZ), &mut done) {
                Err(SleepError::Timeout) => {}
                r => return r.map_err(E::early),
            }
        }
    }
}

impl io_clock {
    /// @sectors of IO done, from task context: into this CPU's batch, or the
    /// clock at once - past a batch, which is a u16, a bucket would wrap it.
    pub fn increment(&self, sectors: u64) {
        let batch = u64::from(IO_CLOCK_PCPU_SECTORS);
        if sectors >= batch {
            return __bch2_increment_clock(self, sectors);
        }

        let v = {
            let cpu = CpuGuard::new();
            let b = self.pcpu_buf.this_cpu(&cpu);
            let v = u64::from(b.get()) + sectors;
            b.set(if v >= batch { 0 } else { v as u16 });
            v
        };
        if v >= batch {
            __bch2_increment_clock(self, v);
        }
    }
}

#[c_export]
pub fn bch2_increment_clock(c: &Opaque<c::bch_fs>, sectors: u64, rw: c_int) {
    Fs::from_c(c).io_clock[rw as usize].increment(sectors)
}

/// The clock moving on @sectors - a CPU's batch, or one IO bigger than a
/// batch, from increment(): wakes the waiters it's passed.
#[c_export]
pub fn __bch2_increment_clock(clock: &c::io_clock, sectors: u64) {
    let now = (clock.now.fetch_add(sectors as i64, Relaxed) as u64).wrapping_add(sectors);

    let mut timers = clock.timers.lock();
    while timers.last().is_some_and(|t| reached(now, t.expire)) {
        let Some(t) = timers.pop() else { break };
        t.waker.wake();
    }
}

impl ToText for io_clock {
    /// The clock, and its waiters, soonest first.
    fn to_text(&self, out: &mut Printbuf) {
        out.tabstop_push(40);
        prt!(out, "current time:\t{}\n", self.now());

        let mut out = out.atomic();
        for t in self.timers.lock().iter().rev() {
            prt!(out, "{}:\t{}\n", t.caller, t.expire);
        }
    }
}

#[pinned_drop]
impl PinnedDrop for io_clock {
    fn drop(self: Pin<&mut Self>) {
        self.project().pcpu_buf.free();
    }
}
