use crate::c;

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

// local_clock() is a static inline and cond_resched() is a macro, so neither
// binds through bindgen directly; util.h wraps both as allowlisted bch2_*
// static inlines that the codegen picks up uniformly on the kernel and
// userspace builds.
pub fn local_clock() -> u64 {
    unsafe { c::bch2_local_clock() }
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
