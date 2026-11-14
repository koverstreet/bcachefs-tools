// SPDX-License-Identifier: GPL-2.0

//! What of the kernel's API both ports have alike: each port's kernel.rs
//! brings it in beside its own.

use crate::c;

use super::PAGE_SIZE;

/// C's READ and WRITE, as indices: io_ref[], io_clock[]...
pub const READ:  usize = 0;
pub const WRITE: usize = 1;

pub const NSEC_PER_SEC: u64 = 1_000_000_000;

/// PAGE_SIZE, for byte positions.
pub const PAGE_BYTES: u64 = PAGE_SIZE as u64;
/// A page's sectors: PAGE_SECTORS, and its shift.
pub const PAGE_SECTORS_SHIFT: usize = PAGE_SIZE.trailing_zeros() as usize - 9;
pub const PAGE_SECTORS: u64 = 1 << PAGE_SECTORS_SHIFT;

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

/// C's prefetch(): a hint, which can't fault, whatever @p is. Into every
/// cache level - prefetcht0, arm64's pldl1keep: what we prefetch is used
/// soon, and maybe again, not streamed past - where the kernel's x86-64
/// prefetch() is prefetchnta, which skips L2, so a line pushed out of L1
/// before its use comes back from L3. Elsewhere, only arm64's is kept. asm!,
/// not _mm_prefetch(): that's an SSE target_feature fn, which the kernel's
/// soft-float target can't inline - it'd be a call.
#[inline(always)]
pub fn prefetch<T>(p: *const T) {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::asm!("prefetcht0 [{}]", in(reg) p, options(nostack, readonly, preserves_flags))
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        core::arch::asm!("prfm pldl1keep, [{}]", in(reg) p, options(nostack, readonly, preserves_flags))
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    let _ = p;
}

pub fn random_u64() -> u64 {
    unsafe { c::get_random_u64() }
}

/// __get_random_u32_below()'s algorithm, widened: the high half of
/// @ceil * random, rejecting the few that would bias it.
pub fn random_u64_below(ceil: u64) -> u64 {
    assert!(ceil > 0);
    let mut m = u128::from(ceil) * u128::from(random_u64());

    if (m as u64) < ceil {
        let bound = ceil.wrapping_neg() % ceil;
        while (m as u64) < bound {
            m = u128::from(ceil) * u128::from(random_u64());
        }
    }

    (m >> 64) as u64
}

// local_clock() is a static inline and cond_resched() a macro, so neither has
// a symbol for Rust: util.c wraps both, as bch2_*, for the kernel and
// userspace builds alike.
pub fn local_clock() -> u64 {
    unsafe { c::local_clock() }
}

pub fn cond_resched() {
    unsafe { c::bch2_cond_resched() }
}

/// C's _RET_IP_: the return address of the function it's expanded in, after
/// inlining - so a macro, not a function. 0 from a rustc before 1.97, which
/// hasn't core::intrinsics::return_address().
#[cfg(bch_return_address)]
#[macro_export]
macro_rules! ret_ip {
    () => { core::intrinsics::return_address() as core::ffi::c_ulong };
}

#[cfg(not(bch_return_address))]
#[macro_export]
macro_rules! ret_ip {
    () => { 0 as core::ffi::c_ulong };
}

/// race_fault(): dynamic_fault("bcachefs:race"), which no kernel we build
/// against has.
pub fn race_fault() -> bool {
    false
}

/// Forward, so @dst may be below an overlapping @src. Raw pointers: through
/// slices, LLVM makes the loop a memcpy() call again.
///
/// # Safety
/// @src and @dst are valid for @n u64s; if they overlap, @dst is at or below
/// @src.
#[inline(always)]
pub unsafe fn copy_u64s_small(dst: *mut u64, src: *const u64, n: usize) {
    let mut i = 0;

    while i + 2 <= n {
        unsafe {
            let (a, b) = (src.add(i).read(), src.add(i + 1).read());
            dst.add(i).write(a);
            dst.add(i + 1).write(b);
        }
        i += 2;
    }
    if i < n {
        unsafe { dst.add(i).write(src.add(i).read()) }
    }
}

pub fn mem_alloc_profiling_enabled() -> bool {
    unsafe { c::mem_alloc_profiling_enabled() }
}

/// For a slice big enough that sorting it would hog the CPU: it yields from
/// the comparisons, the one call of ours the sort makes.
pub fn sort_nonatomic<T>(v: &mut [T], mut cmp: impl FnMut(&T, &T) -> core::cmp::Ordering) {
    let mut n = 0u32;
    v.sort_unstable_by(|a, b| {
        n = n.wrapping_add(1);
        if n % 4096 == 0 {
            cond_resched();
        }
        cmp(a, b)
    })
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

/// Whether jiffies @a is @b or after it, across a wrap: time_after_eq().
pub fn time_after_eq(a: core::ffi::c_ulong, b: core::ffi::c_ulong) -> bool {
    (a.wrapping_sub(b) as core::ffi::c_long) >= 0
}

/// Whether jiffies @a is in [@b, @c], across a wrap: time_in_range().
pub fn time_in_range(a: core::ffi::c_ulong, b: core::ffi::c_ulong, c: core::ffi::c_ulong) -> bool {
    !time_after(b, a) && !time_after(a, c)
}

/// time_after(), of u64 times - local_clock()'s: time_after64().
pub fn time_after64(a: u64, b: u64) -> bool {
    (b.wrapping_sub(a) as i64) < 0
}

/// The kernel's binding returns its own c_ulong, usize; the same width as
/// core's.
pub fn nsecs_to_jiffies(ns: u64) -> core::ffi::c_ulong {
    unsafe { c::nsecs_to_jiffies(ns) as core::ffi::c_ulong }
}

/// Jiffies in @ms milliseconds, rounded up: msecs_to_jiffies().
pub const fn msecs_to_jiffies(ms: core::ffi::c_ulong) -> core::ffi::c_ulong {
    (ms * HZ).div_ceil(1000)
}

/// Milliseconds in @j jiffies: jiffies_to_msecs(), wrapping as its unsigned
/// int does - @j is often a difference of jiffies, racing.
pub const fn jiffies_to_msecs(j: core::ffi::c_ulong) -> core::ffi::c_uint {
    if 1000 % HZ == 0 {
        (1000 / HZ).wrapping_mul(j) as core::ffi::c_uint
    } else {
        (j.wrapping_mul(1000) / HZ) as core::ffi::c_uint
    }
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
    unsafe { c::ktime_get_ns() }
}

/// A byte position as the kernel takes it: a loff_t, past whose top - LLONG_MAX
/// - there's nothing, so u64::MAX is "to the end".
pub fn loff(pos: u64) -> c::loff_t {
    pos.min(i64::MAX as u64) as c::loff_t
}

/// The wall clock, at the coarse clock's granularity: ktime_get_coarse_real_ts64().
pub fn ktime_get_coarse_real_ts64() -> c::timespec64 {
    let mut now: c::timespec64 = pin_init::Zeroable::zeroed();
    unsafe { c::ktime_get_coarse_real_ts64(&mut now) };
    now
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
