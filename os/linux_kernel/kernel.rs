// SPDX-License-Identifier: GPL-2.0

use crate::c;

#[path = "../kernel.rs"]
mod common;
pub use common::*;

pub const PAGE_SIZE: usize = kernel::page::PAGE_SIZE;

/// The @i'th CPU of a spread across the online CPUs, NUMA-local ones first:
/// as cpumask_local_spread(i, NUMA_NO_NODE).
pub fn cpumask_local_spread(i: u32) -> u32 {
    unsafe { c::cpumask_local_spread(i, c::NUMA_NO_NODE) }
}

/// One more than the highest possible CPU number.
pub fn nr_cpu_ids() -> u32 {
    unsafe { c::bch2_nr_cpu_ids() }
}

pub fn is_vmalloc_addr<T>(p: *const T) -> bool {
    unsafe { c::is_vmalloc_addr(p.cast()) }
}

pub fn msleep(ms: u32) {
    unsafe { c::msleep(ms) }
}

pub fn dump_stack() {
    unsafe { c::dump_stack() }
}

/// Whether this task has been sent a fatal signal: as
/// fatal_signal_pending(current).
pub fn fatal_signal_pending() -> bool {
    unsafe { c::bch2_fatal_signal_pending() }
}

/// The jiffies count: a variable the timer interrupt writes, read as
/// READ_ONCE() would.
pub fn jiffies() -> core::ffi::c_ulong {
    // SAFETY: a word, written whole, so a volatile read can't tear
    unsafe { core::ptr::read_volatile(&raw const ::kernel::bindings::jiffies) as core::ffi::c_ulong }
}

pub struct Comm([u8; c::TASK_COMM_LEN as usize]);

impl Comm {
    pub fn as_cstr(&self) -> &core::ffi::CStr {
        core::ffi::CStr::from_bytes_until_nul(&self.0).expect("comm is NUL terminated")
    }
}

impl core::fmt::Display for Comm {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        use core::fmt::Write;
        match self.as_cstr().to_str() {
            Ok(s) => f.write_str(s),
            // A comm is bytes: what isn't ASCII, escaped
            Err(_) => self.as_cstr().to_bytes().iter().try_for_each(|&b| match b.is_ascii() {
                true  => f.write_char(b as char),
                false => write!(f, "\\x{b:02x}"),
            }),
        }
    }
}

pub fn current_comm() -> Comm {
    // SAFETY: our own task's comm, which only we set
    Comm(unsafe { (*crate::os::sched::current()).comm })
}

/// The ids this task's new files are owned by. Its credentials are read as
/// current_cred() reads them, without RCU: only the task itself replaces its
/// own, so they live while it looks.
pub fn current_fsuid() -> c::kuid_t {
    unsafe { (*(*crate::os::sched::current()).cred).fsuid }
}

pub fn current_fsgid() -> c::kgid_t {
    unsafe { (*(*crate::os::sched::current()).cred).fsgid }
}

/// A user task's address space, for a kthread to do the task's IO in.
#[repr(transparent)]
#[derive(Clone, Copy)]
pub struct Mm(core::ptr::NonNull<c::mm_struct>);

impl Mm {
    /// This task's: None for a kthread's.
    pub fn current() -> Option<Self> {
        core::ptr::NonNull::new(unsafe { (*crate::os::sched::current()).mm }).map(Mm)
    }

    /// kthread_use_mm(): this kthread in it, till the guard's dropped.
    ///
    /// # Safety
    /// It's live while the guard is: its task's IO, which this is, holds it.
    pub unsafe fn use_in_kthread(self) -> KthreadMm {
        unsafe { c::kthread_use_mm(self.0.as_ptr()) };
        KthreadMm(self)
    }
}

/// A kthread in a user task's address space: kthread_unuse_mm() on drop.
pub struct KthreadMm(Mm);

impl Drop for KthreadMm {
    fn drop(&mut self) {
        unsafe { c::kthread_unuse_mm(self.0.0.as_ptr()) }
    }
}

/// What this task has run, as of @now: 0 where the scheduler doesn't say.
pub fn current_runtime(now: u64) -> u64 {
    #[cfg(not(any(CONFIG_SCHED_ALT, CONFIG_SCHED_MUQSS)))]
    {
        let se = unsafe { &(*crate::os::sched::current()).se };
        se.sum_exec_runtime.wrapping_add(now.wrapping_sub(se.exec_start))
    }
    /*
     * BMQ/PDS (CONFIG_SCHED_ALT) and MuQSS replace CFS and drop
     * task_struct.se; this is only write-point runtime accounting,
     * so skip it there.
     */
    #[cfg(any(CONFIG_SCHED_ALT, CONFIG_SCHED_MUQSS))]
    {
        let _ = now;
        0
    }
}

/// @bytes written, to this task's io accounting.
pub fn task_io_account_write(bytes: usize) {
    unsafe { c::task_io_account_write(bytes) }
}

/// A number identifying this task while it lives - C's (unsigned long)current
/// - for hashing by task.
pub fn current_id() -> usize {
    crate::os::sched::current() as usize
}

/// The number of CPUs online.
pub fn num_online_cpus() -> u32 {
    unsafe { c::bch2_num_online_cpus() }
}
