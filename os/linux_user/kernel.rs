// SPDX-License-Identifier: GPL-2.0

#[path = "../kernel.rs"]
mod common;
pub use common::*;

pub const PAGE_SIZE: usize = 4096;
/// A file offset is an i64.
pub const MAX_FILE_SIZE: u64 = i64::MAX as u64;

/// Userspace has no vmalloc: its is_vmalloc_addr() was a macro, 0.
pub fn is_vmalloc_addr<T>(_p: *const T) -> bool {
    false
}

/// Errno @err's description, strerror()'s: libc names none.
pub fn errname(err: core::ffi::c_int) -> Option<&'static core::ffi::CStr> {
    let p = unsafe { libc::strerror(err) };
    (!p.is_null()).then(|| unsafe { core::ffi::CStr::from_ptr(p) })
}

/// @buf filled from the OS's CSPRNG. It can't fail - getrandom(2) blocks
/// until the pool is seeded - but for a broken system.
pub fn random_bytes(buf: &mut [u8]) {
    if let Err(e) = getrandom::fill(buf) {
        panic!("getrandom() of {} bytes: {e}", buf.len());
    }
}

pub fn random_u64() -> u64 {
    getrandom::u64().unwrap_or_else(|e| panic!("getrandom() of a u64: {e}"))
}

pub(super) fn sysinfo() -> libc::sysinfo {
    let mut info = core::mem::MaybeUninit::uninit();
    let ret = unsafe { libc::sysinfo(info.as_mut_ptr()) };
    assert_eq!(ret, 0, "sysinfo(): {}", std::io::Error::last_os_error());
    unsafe { info.assume_init() }
}

pub fn totalram_bytes() -> usize {
    let info = sysinfo();
    info.totalram as usize * info.mem_unit as usize
}

/// Free memory: sysinfo() has no MemAvailable.
pub fn mem_available_bytes() -> usize {
    let info = sysinfo();
    info.freeram as usize * info.mem_unit as usize
}

/// No allocation profiling here.
pub fn mem_alloc_profiling_enabled() -> bool {
    false
}

/// Nothing here counts it.
pub fn account_reclaimed(_bytes: usize) {}

pub fn msleep(ms: u32) {
    std::thread::sleep(core::time::Duration::from_millis(ms.into()))
}

pub fn dump_stack() {
    crate::util::backtrace::dump_stack()
}

/// Nothing to yield to that the scheduler wouldn't run anyway.
pub fn cond_resched() {}

/// Always, in userspace.
pub fn capable(_cap: Capability) -> bool {
    true
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

/// A number identifying this thread while it lives, for hashing by task.
pub fn current_id() -> usize {
    std::thread_local!(static ME: u8 = const { 0 });
    ME.with(|me| core::ptr::from_ref(me) as usize)
}

/// How many threads can run at once.
pub fn num_online_cpus() -> u32 {
    std::thread::available_parallelism().map_or(1, |n| n.get() as u32)
}
