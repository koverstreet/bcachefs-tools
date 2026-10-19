// SPDX-License-Identifier: GPL-2.0

//! Per-CPU variables: this_cpu_add(), for Rust - the kernel has none of its
//! own (as of 7.3; a posted series is x86_64 only), so it's ours, the C
//! macros as they were.
//!
//! On x86_64 and arm64 it's inline, as C's is: the asm is the kernel's own,
//! from the headers of the kernel being built against (percpu_asm.rs.S,
//! preprocessed by Makefile.rust), so what the kernel patches at boot - arm64's
//! choices of tpidr_el1 or tpidr_el2, and of LL/SC or LSE atomics - comes
//! from that kernel, never a copy that could fall behind it. x86_64's is one
//! instruction, %gs-relative. Elsewhere, and in userspace - whose per-CPU
//! areas are per thread, found through a C thread-local - it's C's
//! this_cpu_add(), out of line.
//!
//! arm64's doesn't disable preemption, as C's does: it reads this CPU's
//! offset, then adds atomically at it, so a task moved between the two adds
//! to the slot of the CPU it left - atomically, as C's own adds there are.
//! That's an add to "a" CPU's slot, not necessarily this one's: right for a
//! counter summed over every CPU, which is what this is for - not for a
//! per-CPU variable that only its own CPU may touch.

use cstruct_macros::c_extern;

// C's this_cpu_add(), out of line: where Rust has no asm of its own.
c_extern! {
    pub fn bch2_this_cpu_add_u64(pcp: *mut u64, val: u64);
}

#[cfg(all(kernel, any(target_arch = "x86_64", target_arch = "aarch64")))]
macro_rules! concat_literals {
    ($($s:literal)*) => { ::core::concat!($($s),*) };
}

#[cfg(all(kernel, any(target_arch = "x86_64", target_arch = "aarch64")))]
include!(concat!(env!("OUT_DIR"), "/percpu_asm.rs"));

/// Add @val to @pcp, a `u64 __percpu *` - to this CPU's, or on arm64 the
/// one it ran on a moment before (above): this_cpu_add().
///
/// # Safety
/// @pcp is a per-CPU u64, allocated for every possible CPU and live.
#[inline(always)]
pub unsafe fn this_cpu_add(pcp: *mut u64, val: u64) {
    #[cfg(all(kernel, target_arch = "x86_64"))]
    // SAFETY: the caller's - @pcp is a per-CPU address, %gs this CPU's base
    unsafe {
        core::arch::asm!(this_cpu_add_asm!(), ptr = in(reg) pcp, val = in(reg) val,
                         options(att_syntax, nostack));
    }

    #[cfg(all(kernel, target_arch = "aarch64"))]
    // SAFETY: the caller's - @pcp plus this CPU's offset is its slot
    unsafe {
        let off: usize;
        core::arch::asm!(my_cpu_offset_asm!(), off = out(reg) off,
                         options(nomem, nostack, preserves_flags));
        let ptr = pcp.wrapping_byte_add(off);
        core::arch::asm!(this_cpu_add_asm!(), ptr = in(reg) ptr, val = in(reg) val,
                         tmp = out(reg) _, retry = out(reg) _, options(nostack));
    }

    #[cfg(not(all(kernel, any(target_arch = "x86_64", target_arch = "aarch64"))))]
    // SAFETY: the caller's
    unsafe {
        bch2_this_cpu_add_u64(pcp, val);
    }
}

/// Every CPU's @acc.len() u64s at @src, added into @acc: acc_u64s_percpu().
///
/// # Safety
/// @src is a per-CPU array of at least @acc.len() u64s, allocated for every
/// possible CPU and live.
pub unsafe fn acc_u64s_percpu(acc: &mut [u64], src: *const u64) {
    unsafe { crate::c::bch2_acc_u64s_percpu(acc.as_mut_ptr(), src, acc.len() as core::ffi::c_uint) }
}
