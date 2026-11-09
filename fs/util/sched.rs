// SPDX-License-Identifier: GPL-2.0

//! The scheduler's, as Rust calls it: current, schedule(), and for spinning,
//! the clock, need_resched() and whether a task is on a CPU.

use crate::c;
#[cfg(CONFIG_SMP)]
use crate::util::rcu;
#[cfg(CONFIG_SMP)]
use core::ptr::NonNull;

/// This thread's task: current.
#[inline(always)]
pub fn current() -> *mut c::task_struct {
    unsafe { c::bch2_current() }
}

#[inline(always)]
pub fn schedule() {
    unsafe { c::schedule() }
}

/// sched_clock(), in nanoseconds.
#[cfg(CONFIG_SMP)]
#[inline(always)]
pub fn sched_clock() -> u64 {
    unsafe { c::sched_clock() }
}

#[inline(always)]
pub fn need_resched() -> bool {
    unsafe { c::bch2_need_resched() }
}

/// Whether @task is running on a CPU: @task is any task's, read under RCU,
/// which a task's freeing waits for.
#[cfg(CONFIG_SMP)]
#[inline(always)]
pub fn owner_on_cpu(task: NonNull<c::task_struct>, _rcu: &rcu::ReadGuard) -> bool {
    unsafe { c::bch2_owner_on_cpu(task.as_ptr()) }
}

/// Whether this thread is a realtime or deadline task: rt_or_dl_task(current).
#[cfg(CONFIG_SMP)]
#[inline(always)]
pub fn rt_or_dl_task_current() -> bool {
    unsafe { c::bch2_rt_or_dl_task_current() }
}
