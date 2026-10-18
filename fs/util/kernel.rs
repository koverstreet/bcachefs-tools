use crate::c;

/// C's READ and WRITE, as indices: io_ref[], io_clock[]...
pub const READ:  usize = 0;
pub const WRITE: usize = 1;

#[cfg(kernel)]
pub const PAGE_SIZE: usize = kernel::page::PAGE_SIZE;
/// The shim's, include/linux/page.h
#[cfg(not(kernel))]
pub const PAGE_SIZE: usize = 4096;

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

/// Whether this task has been sent a fatal signal: as
/// fatal_signal_pending(current).
pub fn fatal_signal_pending() -> bool {
    unsafe { c::bch2_fatal_signal_pending() }
}

/// A capability, as the kernel's CAP_* - the values are ABI
/// (uapi/linux/capability.h). Defined here because userspace's capable()
/// ignores its argument, so it has no CAP_* constants to bind.
#[derive(Clone, Copy)]
#[repr(i32)]
pub enum Capability {
    Fowner = 3,
}

/// Whether the current task has @cap: as capable(). Always, in userspace.
pub fn capable(cap: Capability) -> bool {
    unsafe { c::bch2_capable(cap as i32) }
}
