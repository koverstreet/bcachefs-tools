// SPDX-License-Identifier: GPL-2.0

#[path = "../kernel.rs"]
mod common;
pub use common::*;

use rustix::time::{clock_gettime, ClockId, Timespec};

pub const PAGE_SIZE: usize = 4096;
/// A file offset is an i64.
pub const MAX_FILE_SIZE: u64 = i64::MAX as u64;

/// The kernel's panic(), as a Rust panic: the process's, with @msg.
pub fn panic(msg: &core::ffi::CStr) -> ! {
    panic!("{}", msg.to_string_lossy())
}

/// Userspace has no vmalloc: its is_vmalloc_addr() was a macro, 0.
pub fn is_vmalloc_addr<T>(_p: *const T) -> bool {
    false
}

/// Errno @err's name - "ENOENT" - as the kernel's errname() has it. Not
/// EWOULDBLOCK or EDEADLOCK: they're EAGAIN and EDEADLK.
pub fn errname(err: i32) -> Option<&'static core::ffi::CStr> {
    macro_rules! names {
        ($($e:ident)*) => {
            match u32::try_from(err).ok()? {
                $(linux_raw_sys::errno::$e => Some(const { crate::util::ffi::c_str(concat!(stringify!($e), "\0")) }),)*
                _ => None,
            }
        };
    }

    names!(
        EPERM ENOENT ESRCH EINTR EIO ENXIO E2BIG ENOEXEC EBADF ECHILD EAGAIN ENOMEM EACCES EFAULT
        ENOTBLK EBUSY EEXIST EXDEV ENODEV ENOTDIR EISDIR EINVAL ENFILE EMFILE ENOTTY ETXTBSY EFBIG
        ENOSPC ESPIPE EROFS EMLINK EPIPE EDOM ERANGE EDEADLK ENAMETOOLONG ENOLCK ENOSYS ENOTEMPTY
        ELOOP ENOMSG EIDRM ECHRNG EL2NSYNC EL3HLT EL3RST ELNRNG EUNATCH ENOCSI EL2HLT EBADE EBADR
        EXFULL ENOANO EBADRQC EBADSLT EBFONT ENOSTR ENODATA ETIME ENOSR ENONET ENOPKG EREMOTE
        ENOLINK EADV ESRMNT ECOMM EPROTO EMULTIHOP EDOTDOT EBADMSG EOVERFLOW ENOTUNIQ EBADFD
        EREMCHG ELIBACC ELIBBAD ELIBSCN ELIBMAX ELIBEXEC EILSEQ ERESTART ESTRPIPE EUSERS ENOTSOCK
        EDESTADDRREQ EMSGSIZE EPROTOTYPE ENOPROTOOPT EPROTONOSUPPORT ESOCKTNOSUPPORT EOPNOTSUPP
        EPFNOSUPPORT EAFNOSUPPORT EADDRINUSE EADDRNOTAVAIL ENETDOWN ENETUNREACH ENETRESET
        ECONNABORTED ECONNRESET ENOBUFS EISCONN ENOTCONN ESHUTDOWN ETOOMANYREFS ETIMEDOUT
        ECONNREFUSED EHOSTDOWN EHOSTUNREACH EALREADY EINPROGRESS ESTALE EUCLEAN ENOTNAM ENAVAIL
        EISNAM EREMOTEIO EDQUOT ENOMEDIUM EMEDIUMTYPE ECANCELED ENOKEY EKEYEXPIRED EKEYREVOKED
        EKEYREJECTED EOWNERDEAD ENOTRECOVERABLE ERFKILL EHWPOISON
    )
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

pub fn totalram_bytes() -> usize {
    let info = rustix::system::sysinfo();
    info.totalram as usize * info.mem_unit as usize
}

/// Free memory: sysinfo() has no MemAvailable.
pub fn mem_available_bytes() -> usize {
    let info = rustix::system::sysinfo();
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
pub fn jiffies() -> crate::util::ffi::c_ulong {
    nsecs_to_jiffies(local_clock())
}

/// Jiffies a second.
pub const HZ: crate::util::ffi::c_ulong = 1000;

pub fn nsecs_to_jiffies(ns: u64) -> crate::util::ffi::c_ulong {
    (ns / (NSEC_PER_SEC / HZ as u64)) as crate::util::ffi::c_ulong
}

fn nsecs(ts: Timespec) -> u64 {
    (ts.tv_sec * NSEC_PER_SEC as i64 + ts.tv_nsec) as u64
}

/// sched_clock(): the coarse monotonic clock, in nanoseconds.
pub fn local_clock() -> u64 {
    nsecs(clock_gettime(ClockId::MonotonicCoarse))
}

/// The monotonic clock, in nanoseconds: ktime_get_ns() - sched_clock() here.
pub fn ktime_get_ns() -> u64 {
    local_clock()
}

/// Wall-clock time, in seconds: ktime_get_real_seconds().
pub fn ktime_get_real_seconds() -> i64 {
    clock_gettime(ClockId::Realtime).tv_sec
}

/// The wall clock, at the coarse clock's granularity: ktime_get_coarse_real_ts64().
pub fn ktime_get_coarse_real_ts64() -> crate::c::timespec64 {
    let ts = clock_gettime(ClockId::RealtimeCoarse);
    crate::c::timespec64 { tv_sec: ts.tv_sec, tv_nsec: ts.tv_nsec as _ }
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
