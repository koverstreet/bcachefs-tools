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

/// The memalloc scope @gfp says, in the flags we use: NORECLAIM without
/// direct reclaim, else NOIO without __GFP_IO; NOWARN. The rest of @gfp has
/// no scope, and goes.
pub fn memalloc_flags_for_gfp(gfp: c::gfp_t) -> u32 {
    use crate::c::{GFP_KERNEL, GFP_NOIO, GFP_NOWAIT, __GFP_NOWARN};
    use crate::locking::{PF_MEMALLOC_NOIO, PF_MEMALLOC_NORECLAIM, PF_MEMALLOC_NOWARN};

    // __GFP_DIRECT_RECLAIM and __GFP_IO, which bindgen doesn't name
    let mut flags = 0;
    if gfp & (GFP_NOIO & !GFP_NOWAIT) == 0 {
        flags |= PF_MEMALLOC_NORECLAIM;
    } else if gfp & (GFP_KERNEL & !GFP_NOIO) == 0 {
        flags |= PF_MEMALLOC_NOIO;
    }
    if gfp & __GFP_NOWARN != 0 {
        flags |= PF_MEMALLOC_NOWARN;
    }
    flags
}

/// Caught taken where it can't sleep, as C's might_sleep(): at the caller.
#[track_caller]
#[inline]
pub fn might_sleep() {
    ::kernel::task::might_sleep()
}

/// # Safety
/// @task is a task, live.
pub unsafe fn task_pid(task: *mut c::task_struct) -> core::ffi::c_int {
    unsafe { c::bch2_task_pid(task) }
}

/// The CPU this task's to be woken on next, for the scheduler.
pub fn set_current_wake_cpu(cpu: core::ffi::c_int) {
    unsafe { c::bch2_set_current_wake_cpu(cpu) }
}

/// Whether a signal @state's sleep wakes for is pending: as
/// signal_pending_state(state, current).
pub fn signal_pending_state(state: core::ffi::c_uint) -> bool {
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
