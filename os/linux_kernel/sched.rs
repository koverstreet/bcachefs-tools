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
    crate::os::percpu::current_task()
}

#[inline(always)]
pub fn schedule() {
    unsafe { c::schedule() }
}

pub fn schedule_timeout(timeout: crate::util::ffi::c_long) -> crate::util::ffi::c_long {
    unsafe { c::schedule_timeout(timeout as _) as _ }
}

pub fn io_schedule_timeout(timeout: crate::util::ffi::c_long) -> crate::util::ffi::c_long {
    unsafe { c::io_schedule_timeout(timeout as _) as _ }
}

/// sched_clock(), in nanoseconds.
#[cfg(CONFIG_SMP)]
#[inline(always)]
pub fn sched_clock() -> u64 {
    unsafe { c::sched_clock() }
}

/* TIF_NEED_RESCHED in current's thread_info, the head of its task_struct */
#[inline(always)]
pub fn need_resched() -> bool {
    let flags = unsafe { core::ptr::read_volatile(&raw const (*current()).thread_info.flags) };
    flags & (1 << c::TIF_NEED_RESCHED) != 0
}

/// current->flags: the memalloc scopes the task is in.
pub fn memalloc_flags() -> u32 {
    unsafe { (*current()).flags }
}

/// This task's state, @state, ordered before what it then checks:
/// set_current_state().
pub fn set_current_state(state: u32) {
    unsafe { c::bch2_set_current_state(state) }
}

/// This task running again: __set_current_state(TASK_RUNNING).
pub fn set_current_state_running() {
    unsafe { c::bch2_set_current_state_running() }
}

/// Caught taken where it can't sleep, as C's might_sleep(): at the caller.
#[track_caller]
#[inline]
pub fn might_sleep() {
    ::kernel::task::might_sleep()
}

/// # Safety
/// @task is a task, live.
pub unsafe fn task_pid(task: *mut c::task_struct) -> i32 {
    unsafe { c::bch2_task_pid(task) }
}

pub fn current_pid() -> i32 {
    unsafe { task_pid(current()) }
}

/// The CPU this task's to be woken on next, for the scheduler.
pub fn set_current_wake_cpu(cpu: i32) {
    unsafe { c::bch2_set_current_wake_cpu(cpu) }
}

/// Whether a signal @state's sleep wakes for is pending: as
/// signal_pending_state(state, current).
pub fn signal_pending_state(state: u32) -> bool {
    unsafe { c::bch2_signal_pending_state(state) }
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
