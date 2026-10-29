use crate::c;

/// C's READ and WRITE, as indices: io_ref[], io_clock[]...
pub const READ:  usize = 0;
pub const WRITE: usize = 1;

pub const NSEC_PER_SEC: u64 = 1_000_000_000;

#[cfg(kernel)]
pub const PAGE_SIZE: usize = kernel::page::PAGE_SIZE;
/// The shim's, include/linux/page.h
#[cfg(not(kernel))]
pub const PAGE_SIZE: usize = 4096;

/// C's unlikely(): @b, and the branch it decides is a slowpath to the
/// compiler - laid out of line, kept out of the fastpath's registers.
#[inline(always)]
pub fn unlikely(b: bool) -> bool {
    if b {
        cold_path();
    }
    b
}

/// C's likely(): @b, and the branch it decides against is the slowpath.
#[inline(always)]
pub fn likely(b: bool) -> bool {
    if !b {
        cold_path();
    }
    b
}

/// The path that reaches this is rarely taken: core::hint::cold_path(), which
/// is Rust 1.95 and our MSRV is below it. A call that survives MIR inlining,
/// so both backends see it: LLVM deletes the call and keeps the branch weights,
/// cg_clif marks the block cold.
#[cold]
#[inline(never)]
pub fn cold_path() {}

/// Ask @shrink to free @nr objects, as memory pressure would: its
/// scan_objects(), GFP_KERNEL.
///
/// # Safety
/// @shrink is a live shrinker.
pub unsafe fn shrinker_scan(shrink: *mut c::shrinker, nr: core::ffi::c_ulong) {
    // SAFETY: integers and a pointer, each valid as zero
    let mut sc: c::shrink_control = unsafe { core::mem::zeroed() };
    sc.gfp_mask = c::GFP_KERNEL;
    sc.nr_to_scan = nr as _;

    let scan = unsafe { (*shrink).scan_objects }.expect("shrinker without scan_objects");
    unsafe { scan(shrink, &mut sc) };
}

/// The @i'th CPU of a spread across the online CPUs, NUMA-local ones first:
/// as cpumask_local_spread(i, NUMA_NO_NODE).
#[cfg(kernel)]
pub fn cpumask_local_spread(i: u32) -> u32 {
    unsafe { c::bch2_cpumask_local_spread(i) }
}

/// One more than the highest possible CPU number.
#[cfg(kernel)]
pub fn nr_cpu_ids() -> u32 {
    unsafe { c::bch2_nr_cpu_ids() }
}

/// The smallest IO @bdev takes: bdev_logical_block_size().
pub fn bdev_logical_block_size(bdev: &c::block_device) -> u32 {
    // SAFETY: a block device's queue, for as long as it's open
    #[cfg(kernel)]
    return unsafe { (*bdev.bd_queue).limits.logical_block_size };
    // SAFETY: as the kernel's
    #[cfg(not(kernel))]
    return unsafe { c::bdev_logical_block_size(core::ptr::from_ref(bdev).cast_mut()) };
}

/// The most @bdev discards at once, in sectors - 0 if it doesn't:
/// bdev_max_discard_sectors(). Userspace discards nothing, as its shim says.
pub fn bdev_max_discard_sectors(bdev: &c::block_device) -> u32 {
    // SAFETY: a block device's queue, for as long as it's open
    #[cfg(kernel)]
    return unsafe { (*bdev.bd_queue).limits.max_discard_sectors };
    #[cfg(not(kernel))]
    {
        let _ = bdev;
        0
    }
}

pub fn random_u64() -> u64 {
    unsafe { c::bch2_get_random_u64() }
}

pub fn random_u64_below(ceil: u64) -> u64 {
    assert!(ceil > 0);
    unsafe { c::bch2_get_random_u64_below(ceil) }
}

// local_clock() is a static inline and cond_resched() a macro, so neither has
// a symbol for Rust: util.c wraps both, as bch2_*, for the kernel and
// userspace builds alike.
pub fn local_clock() -> u64 {
    unsafe { c::bch2_local_clock() }
}

pub fn cond_resched() {
    unsafe { c::bch2_cond_resched() }
}

pub fn msleep(ms: u32) {
    unsafe { c::bch2_msleep(ms) }
}

/// The system's memory, in bytes: totalram_pages() of them.
pub fn totalram_bytes() -> usize {
    unsafe { c::totalram_pages() as usize * PAGE_SIZE }
}

/// Jiffies a second.
pub const HZ: core::ffi::c_ulong = c::HZ as core::ffi::c_ulong;

/// Whether jiffies @a is after @b, across a wrap: time_after().
pub fn time_after(a: core::ffi::c_ulong, b: core::ffi::c_ulong) -> bool {
    (b.wrapping_sub(a) as core::ffi::c_long) < 0
}

/// The kernel's binding returns its own c_ulong, usize; the same width as
/// core's.
pub fn nsecs_to_jiffies(ns: u64) -> core::ffi::c_ulong {
    unsafe { c::nsecs_to_jiffies(ns) as core::ffi::c_ulong }
}

/// The jiffies count. In the kernel it's a variable the timer interrupt
/// writes, read as READ_ONCE() would; in userspace C's macro computes it from
/// sched_clock(), which is local_clock() there.
pub fn jiffies() -> core::ffi::c_ulong {
    #[cfg(kernel)]
    // SAFETY: a word, written whole, so a volatile read can't tear
    let j = unsafe { core::ptr::read_volatile(&raw const ::kernel::bindings::jiffies) as core::ffi::c_ulong };
    #[cfg(not(kernel))]
    let j = nsecs_to_jiffies(local_clock());
    j
}

#[cfg(kernel)]
pub struct Comm([u8; c::TASK_COMM_LEN as usize]);

#[cfg(kernel)]
impl Comm {
    pub fn as_cstr(&self) -> &core::ffi::CStr {
        core::ffi::CStr::from_bytes_until_nul(&self.0).expect("comm is NUL terminated")
    }
}

#[cfg(kernel)]
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

#[cfg(kernel)]
pub fn current_comm() -> Comm {
    // SAFETY: our own task's comm, which only we set
    Comm(unsafe { (*c::get_current()).comm })
}

/// The ids this task's new files are owned by. Its credentials are read as
/// current_cred() reads them, without RCU: only the task itself replaces its
/// own, so they live while it looks.
#[cfg(kernel)]
pub fn current_fsuid() -> c::kuid_t {
    unsafe { (*(*c::get_current()).cred).fsuid }
}

#[cfg(kernel)]
pub fn current_fsgid() -> c::kgid_t {
    unsafe { (*(*c::get_current()).cred).fsgid }
}

/// A number identifying this task while it lives - C's (unsigned long)current
/// - for hashing by task. In userspace, the thread's.
pub fn current_id() -> usize {
    #[cfg(kernel)]
    // SAFETY: reads this CPU's current task, which is us
    let id = unsafe { c::get_current() } as usize;
    #[cfg(not(kernel))]
    let id = {
        std::thread_local!(static ME: u8 = const { 0 });
        ME.with(|me| core::ptr::from_ref(me) as usize)
    };
    id
}

/// Whether this task has been sent a fatal signal: as
/// fatal_signal_pending(current).
pub fn fatal_signal_pending() -> bool {
    unsafe { c::bch2_fatal_signal_pending() }
}

/// Wall-clock time, in seconds: ktime_get_real_seconds().
pub fn ktime_get_real_seconds() -> i64 {
    unsafe { c::ktime_get_real_seconds() as i64 }
}

/// The monotonic clock, in nanoseconds: ktime_get_ns().
pub fn ktime_get_ns() -> u64 {
    unsafe { c::bch2_ktime_get_ns() }
}

/// The number of CPUs online; in userspace, how many threads can run at once.
pub fn num_online_cpus() -> u32 {
    #[cfg(kernel)]
    let n = unsafe { c::bch2_num_online_cpus() };
    #[cfg(not(kernel))]
    let n = std::thread::available_parallelism().map_or(1, |n| n.get() as u32);
    n
}

/// A capability, as the kernel's CAP_* - the values are ABI
/// (uapi/linux/capability.h). Defined here because userspace's capable()
/// ignores its argument, so it has no CAP_* constants to bind.
#[derive(Clone, Copy)]
#[repr(i32)]
pub enum Capability {
    Fowner = 3,
    SysAdmin = 21,
}

/// Whether the current task has @cap: as capable(). Always, in userspace.
pub fn capable(cap: Capability) -> bool {
    unsafe { c::bch2_capable(cap as i32) }
}
