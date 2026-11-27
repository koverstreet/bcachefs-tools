// SPDX-License-Identifier: GPL-2.0

use crate::c;
use crate::posix::{dev::dev_t, mode::umode_t};

#[path = "../kernel.rs"]
mod common;
pub use common::*;

pub const PAGE_SIZE: usize = kernel::page::PAGE_SIZE;
pub const MAX_FILE_SIZE: u64 = c::MAX_LFS_FILESIZE as u64;

/// The kernel panicked, with @msg: panic().
pub fn panic(msg: &core::ffi::CStr) -> ! {
    unsafe { c::panic(c"%s".as_ptr().cast(), msg.as_ptr()) }
}

/// The @i'th CPU of a spread across the online CPUs, NUMA-local ones first:
/// as cpumask_local_spread(i, NUMA_NO_NODE).
pub fn cpumask_local_spread(i: u32) -> u32 {
    unsafe { c::cpumask_local_spread(i, c::NUMA_NO_NODE) }
}

/// One more than the highest possible CPU number: NR_CPUS, as cpumask.h has
/// it, where there's no variable.
#[inline]
pub fn nr_cpu_ids() -> u32 {
    #[cfg(all(CONFIG_SMP, not(CONFIG_FORCE_NR_CPUS)))]
    return unsafe { c::nr_cpu_ids };
    #[cfg(not(all(CONFIG_SMP, not(CONFIG_FORCE_NR_CPUS))))]
    c::NR_CPUS
}

pub fn totalram_bytes() -> usize {
    unsafe { c::totalram_pages() as usize * PAGE_SIZE }
}

pub fn mem_available_bytes() -> usize {
    unsafe { c::si_mem_available() as usize * PAGE_SIZE }
}

pub fn mem_alloc_profiling_enabled() -> bool {
    unsafe { c::mem_alloc_profiling_enabled() }
}

/// @bytes freed, for reclaim to count as its own.
pub fn account_reclaimed(bytes: usize) {
    unsafe { c::mm_account_reclaimed_pages((bytes / PAGE_SIZE) as _) }
}

pub fn random_bytes(buf: &mut [u8]) {
    unsafe { c::get_random_bytes(buf.as_mut_ptr().cast(), buf.len() as _) }
}

pub fn random_u64() -> u64 {
    unsafe { c::get_random_u64() }
}

pub fn is_vmalloc_addr<T>(p: *const T) -> bool {
    unsafe { c::is_vmalloc_addr(p.cast()) }
}

/// Errno @err's name: "ENOENT".
pub fn errname(err: i32) -> Option<&'static core::ffi::CStr> {
    let p = unsafe { c::errname(err) };
    (!p.is_null()).then(|| unsafe { core::ffi::CStr::from_ptr(p.cast()) })
}

pub fn msleep(ms: u32) {
    unsafe { c::msleep(ms) }
}

pub fn dump_stack() {
    unsafe { c::dump_stack() }
}

pub fn cond_resched() {
    unsafe { c::bch2_cond_resched() }
}

/// Whether the current task has @cap: as capable().
pub fn capable(cap: Capability) -> bool {
    unsafe { c::bch2_capable(cap as i32) }
}

/// Whether this task has been sent a fatal signal: as
/// fatal_signal_pending(current).
pub fn fatal_signal_pending() -> bool {
    unsafe { c::bch2_fatal_signal_pending() }
}

/// The jiffies count: a variable the timer interrupt writes, read as
/// READ_ONCE() would.
pub fn jiffies() -> crate::util::ffi::c_ulong {
    // SAFETY: a word, written whole, so a volatile read can't tear
    unsafe { core::ptr::read_volatile(&raw const ::kernel::bindings::jiffies) as crate::util::ffi::c_ulong }
}

/// Jiffies a second.
pub const HZ: crate::util::ffi::c_ulong = c::HZ as crate::util::ffi::c_ulong;

/// The kernel's binding returns its own c_ulong, usize; the same width as
/// core's.
pub fn nsecs_to_jiffies(ns: u64) -> crate::util::ffi::c_ulong {
    unsafe { c::nsecs_to_jiffies(ns) as crate::util::ffi::c_ulong }
}

// local_clock() is a static inline, with no symbol for Rust: the bindings'
// wrapper of it.
pub fn local_clock() -> u64 {
    unsafe { c::local_clock() }
}

/// The monotonic clock, in nanoseconds: ktime_get_ns().
pub fn ktime_get_ns() -> u64 {
    unsafe { c::ktime_get_ns() }
}

/// Wall-clock time, in seconds: ktime_get_real_seconds().
pub fn ktime_get_real_seconds() -> i64 {
    unsafe { c::ktime_get_real_seconds() as i64 }
}

/// The wall clock, at the coarse clock's granularity: ktime_get_coarse_real_ts64().
pub fn ktime_get_coarse_real_ts64() -> c::timespec64 {
    let mut now: c::timespec64 = pin_init::Zeroable::zeroed();
    unsafe { c::ktime_get_coarse_real_ts64(&mut now) };
    now
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

/// The mode bits this task's new files are made without.
pub fn current_umask() -> umode_t {
    umode_t(unsafe { c::current_umask() } as u16)
}

/// The kernel's device number @dev is: MAJOR() and MINOR(), whatever the
/// kernel's layout.
pub fn dev_from_kernel(dev: c::dev_t) -> dev_t {
    dev_t::new(dev >> c::MINORBITS, dev & ((1 << c::MINORBITS) - 1))
}

/// @dev as the kernel's: MKDEV().
pub fn dev_to_kernel(dev: dev_t) -> c::dev_t {
    dev.major() << c::MINORBITS | dev.minor()
}

/// Whether this syscall is a 32-bit task's, on a 64-bit kernel.
pub fn in_compat_syscall() -> bool {
    unsafe { c::in_compat_syscall() }
}

/// A filesystem id made of @uuid, as statfs() reports it.
pub fn uuid_to_fsid(uuid: &[u8; 16]) -> c::__kernel_fsid_t {
    unsafe { c::uuid_to_fsid(uuid.as_ptr().cast_mut()) }
}

/// A user task's address space, for a kthread to do the task's IO in.
#[repr(transparent)]
#[derive(Clone, Copy)]
pub struct Mm(core::ptr::NonNull<c::mm_struct>);

// for another thread to use: what it's for
unsafe impl Send for Mm {}

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
