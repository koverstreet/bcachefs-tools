// SPDX-License-Identifier: GPL-2.0

#[path = "../kernel.rs"]
mod common;
pub use common::*;

/// The shim's, include/linux/page.h
pub const PAGE_SIZE: usize = 4096;

/// Userspace has no vmalloc: its is_vmalloc_addr() was a macro, 0.
pub fn is_vmalloc_addr<T>(_p: *const T) -> bool {
    false
}

pub fn msleep(ms: u32) {
    std::thread::sleep(core::time::Duration::from_millis(ms.into()))
}

pub fn dump_stack() {
    crate::util::backtrace::dump_stack()
}

/// Nothing signals a thread here.
pub fn fatal_signal_pending() -> bool {
    false
}

/// The jiffies count: C's macro computed it from sched_clock(), which is
/// local_clock() here.
pub fn jiffies() -> core::ffi::c_ulong {
    nsecs_to_jiffies(local_clock())
}

/// What this task has run, as of @now: 0, with no scheduler to say.
pub fn current_runtime(_now: u64) -> u64 {
    0
}

/// A number identifying this thread while it lives, for hashing by task.
pub fn current_id() -> usize {
    std::thread_local!(static ME: u8 = const { 0 });
    ME.with(|me| core::ptr::from_ref(me) as usize)
}

/// How many threads can run at once.
pub fn num_online_cpus() -> u32 {
    std::thread::available_parallelism().map_or(1, |n| n.get() as u32)
}
