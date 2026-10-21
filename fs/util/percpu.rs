// SPDX-License-Identifier: GPL-2.0

//! Per-CPU counters: PerCpu<T>, a `T __percpu *` as C's structs hold it,
//! added to on this CPU and summed over every possible one. The kernel has no
//! per-CPU operations for Rust (as of 7.3; a posted series is x86_64 only), so
//! they're ours, the C macros as they were.
//!
//! A sum is a fold of each CPU's copy, read volatile - no reference into
//! memory its CPU is writing - by the T's Counters: wrapping, as the deltas
//! are signed, and sum_nonnegative() clamped, for reads that race the adds.
//!
//! The add on x86_64 and arm64 is inline, as C's is: the asm is the kernel's
//! own, from the headers of the kernel being built against (percpu_asm.rs.S,
//! preprocessed by Makefile.rust), so what the kernel patches at boot - arm64's
//! choices of tpidr_el1 or tpidr_el2, and of LL/SC or LSE atomics - comes
//! from that kernel, never a copy that could fall behind it. x86_64's is one
//! instruction, %gs-relative. Elsewhere, and in userspace - whose per-CPU
//! areas are per thread, found through a C thread-local - it's C's
//! this_cpu_add(), out of line; per_cpu_ptr() and the possible CPUs are C's
//! everywhere.
//!
//! arm64's doesn't disable preemption, as C's does: it reads this CPU's
//! offset, then adds atomically at it, so a task moved between the two adds
//! to the slot of the CPU it left - atomically, as C's own adds there are.
//! That's an add to "a" CPU's slot, not necessarily this one's: right for a
//! counter summed over every CPU, which is what this is for - not for a
//! per-CPU variable that only its own CPU may touch.

use core::ffi::{c_int, c_uint, c_void};
use core::marker::PhantomData;
use cstruct_macros::c_extern;

// C's this_cpu_add() - out of line, where Rust has no asm of its own - and
// percpu_u64_set(), per_cpu_ptr() and the possible CPUs.
c_extern! {
    pub fn bch2_this_cpu_add_u64(pcp: *mut u64, val: u64);
    pub fn bch2_percpu_u64_set(pcp: *mut u64, val: u64);
    pub fn bch2_per_cpu_ptr(pcp: *mut c_void, cpu: c_uint) -> *mut c_void;
    pub fn bch2_cpu_possible_next(cpu: c_int) -> c_int;
}

#[cfg(all(kernel, any(target_arch = "x86_64", target_arch = "aarch64")))]
include!(concat!(env!("OUT_DIR"), "/percpu_asm.rs"));

/// Add @val to @pcp, a `u64 __percpu *` - to this CPU's, or on arm64 the
/// one it ran on a moment before (above): this_cpu_add().
///
/// # Safety
/// @pcp is a per-CPU u64, allocated for every possible CPU and live.
#[inline(always)]
pub(crate) unsafe fn this_cpu_add(pcp: *mut u64, val: u64) {
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

/// The possible CPUs: for_each_possible_cpu().
fn possible_cpus() -> impl Iterator<Item = c_uint> {
    let mut cpu: c_int = -1;
    core::iter::from_fn(move || {
        cpu = unsafe { bch2_cpu_possible_next(cpu) };
        (cpu >= 0).then_some(cpu as c_uint)
    })
}

/// A per-CPU @T: C's `T __percpu *`, a field of a struct C shares.
///
/// C allocates it at init and frees it at exit; between, it's live for every
/// possible CPU, which the methods take as given - as C's per-CPU operations
/// do. Not an address: an offset into each CPU's area. Not Copy: a copy could
/// outlive the struct, where a PerCpuRef borrows it.
#[repr(transparent)]
pub struct PerCpu<T>(*mut T);

// Null: not yet allocated, as C's init finds it.
unsafe impl<T> ::pin_init::Zeroable for PerCpu<T> {}

/// A PerCpu, or a part of one, for as long as it's borrowed.
pub struct PerCpuRef<'a, T>(*mut T, PhantomData<&'a PerCpu<T>>);

impl<T> Clone for PerCpuRef<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for PerCpuRef<'_, T> {}

impl<T> PerCpu<T> {
    pub fn get(&self) -> PerCpuRef<'_, T> {
        PerCpuRef(self.0, PhantomData)
    }

    /// The per-CPU address: for io_done_add(), until fields project.
    pub(crate) fn as_ptr(&self) -> *mut T {
        self.0
    }
}

impl<T, const N: usize> PerCpu<[T; N]> {
    /// Element @i of each CPU's array.
    pub fn at(&self, i: usize) -> PerCpuRef<'_, T> {
        assert!(i < N);
        PerCpuRef(self.0.cast::<T>().wrapping_add(i), PhantomData)
    }
}

impl<T: Counters> PerCpu<T> {
    pub fn sum(&self) -> T {
        self.get().sum()
    }

    pub fn sum_nonnegative(&self) -> T {
        self.get().sum_nonnegative()
    }
}

impl<T: Counters> PerCpuRef<'_, T> {
    /// @cpu's copy, read volatile: its CPU may be writing it.
    fn read(&self, cpu: c_uint) -> T {
        unsafe { core::ptr::read_volatile(bch2_per_cpu_ptr(self.0.cast(), cpu).cast::<T>()) }
    }

    /// Every CPU's, summed: exact only with the adders excluded.
    pub fn sum(&self) -> T {
        let mut cpus = possible_cpus();
        let first = cpus.next().expect("CPU 0 is always possible");
        cpus.fold(self.read(first), |acc, cpu| acc.wrapping_add(self.read(cpu)))
    }

    /// sum(), each counter clamped at 0: for a read that races the adds.
    pub fn sum_nonnegative(&self) -> T {
        self.sum().clamp_nonnegative()
    }
}

impl PerCpuRef<'_, u64> {
    /// Add @v to this CPU's: this_cpu_add().
    #[inline(always)]
    pub fn add(&self, v: u64) {
        unsafe { this_cpu_add(self.0, v) }
    }

    /// This CPU's to @v, every other's to 0.
    ///
    /// # Safety
    /// An add racing it may or may not count. Whether that's sound is up to
    /// what the variable is for - or there are no adders yet.
    pub unsafe fn set(&self, v: u64) {
        unsafe { bch2_percpu_u64_set(self.0, v) }
    }
}

/// Per-CPU counters: a u64, or a struct or array of them, summed over the
/// CPUs. Each CPU's count is a signed delta - added to on one CPU, taken off
/// on another - so the additions wrap, and a sum taken while they change can
/// be negative: as a u64, nearly 2^64. A struct gets it with
/// #[fieldwise(Counters::wrapping_add(rhs), Counters::clamp_nonnegative)].
pub trait Counters: Copy {
    fn wrapping_add(self, rhs: Self) -> Self;
    fn clamp_nonnegative(self) -> Self;
}

impl Counters for u64 {
    #[inline]
    fn wrapping_add(self, rhs: u64) -> u64 {
        u64::wrapping_add(self, rhs)
    }

    #[inline]
    fn clamp_nonnegative(self) -> u64 {
        (self as i64).max(0) as u64
    }
}

impl<T: Counters, const N: usize> Counters for [T; N] {
    #[inline]
    fn wrapping_add(self, rhs: Self) -> Self {
        core::array::from_fn(|i| self[i].wrapping_add(rhs[i]))
    }

    #[inline]
    fn clamp_nonnegative(self) -> Self {
        self.map(Counters::clamp_nonnegative)
    }
}
