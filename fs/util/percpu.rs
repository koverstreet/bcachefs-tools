// SPDX-License-Identifier: GPL-2.0

//! Per-CPU data in the structs C allocates it in - a `T __percpu *` field -
//! typed and operated on from Rust. The kernel has no per-CPU operations for
//! Rust (as of 7.3 a posted series is x86_64 only, and userspace has none), so
//! these are ours: C's operations, with C's contract.
//!
//! Per-CPU data is of two kinds, which want different things:
//!
//! - Counters - counters.now, io_done, ca->usage, capacity.pcpu's usage and
//!   online_reserved: added to on whatever CPU the adder is on, from any
//!   context, and only read as the sum over every CPU. Each CPU's copy is a
//!   signed delta - a reservation taken on one CPU is put on another - so the
//!   adds wrap, and a sum is exact only with the adders excluded: one that
//!   races them can come out negative. Counters says how a T sums - wrapping,
//!   and clamped for racing reads - field by field for a struct, by
//!   #[derive(Fieldwise)].
//!
//! - Slots a CPU owns - capacity.pcpu's reservation caches, a cache of a
//!   freed object: changed by their own CPU, by a read-modify-write or an
//!   xchg or cmpxchg, and at most reset by another.
//!
//! The types:
//!
//! - PerCpu<T> is the field. C allocates it at init and frees it at exit;
//!   between, it's live for every possible CPU, which every operation takes
//!   as given, as C's do. Not Copy: nothing holds it past its struct.
//! - PerCpuRef<'a, T> borrows it, or a part of it - at(i), percpu_field!() -
//!   as every CPU's copy: sum() over all of them; add(), set(), xchg() and
//!   cmpxchg() on this CPU's, each one operation.
//! - ThisCpu<'g, T> is this CPU's copy, while a CpuGuard - preemption off -
//!   keeps it this CPU: a place, as util/projections.rs's are, for a slot's
//!   read-modify-write, or add() of a delta to a struct of counters.
//!
//! What's atomic is C's contract and no more: a this_cpu operation is whole
//! against this CPU - its interrupts and preemption - not against another CPU
//! writing the same copy. arm64's happen to be, x86's aren't, so nothing may
//! lean on it. ThisCpu's read-modify-write is a load and a store, as C's
//! under preempt_disable() are: an interrupt adding to the same copy between
//! them is lost, so a counter interrupts add to goes through PerCpuRef::add().
//! Another CPU's copy is only read volatile, never through a reference: it
//! may be being written.
//!
//! A per-CPU pointer isn't an address - on x86 it's an offset from %gs - so
//! nothing projects through one: a field's offset is taken in a T on the
//! stack, and a CPU's copy is found by its CPU's offset.
//!
//! On x86_64 and arm64 the operations are inline, as C's are, their asm the
//! kernel's own: from the headers of the kernel built against
//! (percpu_asm.rs.S, preprocessed by Makefile.rust), so what that kernel
//! patches at boot - arm64's tpidr_el1 or tpidr_el2, its LL/SC or LSE - is
//! that kernel's, never a copy falling behind it.
//!
//! - x86_64: one %gs-relative instruction - add, cmpxchg, and xchg as C's
//!   cmpxchg loop - none LOCKed.
//! - arm64: this CPU's offset from tpidr, then the kernel's add, LSE's ldadd
//!   or LL/SC, or util/atomic.rs's swp or cas for xchg and cmpxchg. The add
//!   leaves preemption on, where C's disables it: a task moved between
//!   reading the offset and adding adds, atomically, to the CPU it left -
//!   right for a counter, which is only ever summed. xchg and cmpxchg disable
//!   it, as C's do.
//! - Elsewhere, and in userspace - whose per-CPU areas are per thread, found
//!   through a C thread-local - C's, out of line.
//!
//! C's everywhere, out of line: per_cpu_ptr() and the possible CPUs, for the
//! sums; preempt_disable() and _enable(), for CpuGuard; this_cpu_ptr() but on
//! arm64. Around a ThisCpu on x86 that's three calls where C makes none - a
//! cost for a slowpath, not yet for anything hotter.

use core::ffi::{c_int, c_uint, c_void};
use core::marker::PhantomData;
use core::mem::MaybeUninit;
use cstruct_macros::c_extern;

use crate::util::projections::{FakePlaceRef, FakePlaceWrite, FakePlaceRead};

c_extern! {
    pub fn bch2_this_cpu_add_u64(pcp: *mut u64, val: u64);
    pub fn bch2_this_cpu_xchg_u64(pcp: *mut u64, val: u64) -> u64;
    pub fn bch2_this_cpu_cmpxchg_u64(pcp: *mut u64, old: u64, new: u64) -> u64;
    pub fn bch2_this_cpu_ptr(pcp: *mut c_void) -> *mut c_void;
    pub fn bch2_per_cpu_ptr(pcp: *mut c_void, cpu: c_uint) -> *mut c_void;
    pub fn bch2_cpu_possible_next(cpu: c_int) -> c_int;
    pub fn bch2_preempt_disable();
    pub fn bch2_preempt_enable();
}

// ── The operations, an arch's own ────────────────────────────────────────
//
// Safety, for each: @pcp is a per-CPU u64 - a T, for this_cpu_ptr() -
// allocated for every possible CPU and live.

#[cfg(all(kernel, target_arch = "x86_64"))]
mod arch {
    use super::{bch2_this_cpu_ptr, CpuGuard};

    include!(concat!(env!("OUT_DIR"), "/percpu_asm.rs"));

    #[inline(always)]
    pub(super) unsafe fn this_cpu_add(pcp: *mut u64, val: u64) {
        unsafe {
            core::arch::asm!(this_cpu_add_asm!(), ptr = in(reg) pcp, val = in(reg) val,
                             options(att_syntax, nostack));
        }
    }

    #[inline(always)]
    pub(super) unsafe fn this_cpu_ptr<T>(pcp: *mut T, _g: &CpuGuard) -> *mut T {
        unsafe { bch2_this_cpu_ptr(pcp.cast()).cast() }
    }

    #[inline(always)]
    pub(super) unsafe fn this_cpu_xchg(pcp: *mut u64, val: u64) -> u64 {
        let old: u64;
        unsafe {
            core::arch::asm!(this_cpu_xchg_asm!(), ptr = in(reg) pcp, new = in(reg) val,
                             out("rax") old, options(att_syntax, nostack));
        }
        old
    }

    #[inline(always)]
    pub(super) unsafe fn this_cpu_cmpxchg(pcp: *mut u64, old: u64, new: u64) -> u64 {
        let prev: u64;
        unsafe {
            core::arch::asm!(this_cpu_cmpxchg_asm!(), ptr = in(reg) pcp, new = in(reg) new,
                             inout("rax") old => prev, options(att_syntax, nostack));
        }
        prev
    }
}

#[cfg(all(kernel, target_arch = "aarch64"))]
mod arch {
    use super::CpuGuard;
    use core::sync::atomic::{AtomicU64, Ordering};

    include!(concat!(env!("OUT_DIR"), "/percpu_asm.rs"));

    /// This CPU's copy of @pcp - while the task stays on it.
    #[inline(always)]
    fn my_cpu<T>(pcp: *mut T) -> *mut T {
        let off: usize;
        // SAFETY: reads tpidr, as the kernel patched it
        unsafe {
            core::arch::asm!(my_cpu_offset_asm!(), off = out(reg) off,
                             options(nomem, nostack, preserves_flags));
        }
        pcp.wrapping_byte_add(off)
    }

    #[inline(always)]
    pub(super) unsafe fn this_cpu_add(pcp: *mut u64, val: u64) {
        unsafe {
            core::arch::asm!(this_cpu_add_asm!(), ptr = in(reg) my_cpu(pcp), val = in(reg) val,
                             tmp = out(reg) _, retry = out(reg) _, options(nostack));
        }
    }

    #[inline(always)]
    pub(super) unsafe fn this_cpu_ptr<T>(pcp: *mut T, _g: &CpuGuard) -> *mut T {
        my_cpu(pcp)
    }

    #[inline(always)]
    pub(super) unsafe fn this_cpu_xchg(pcp: *mut u64, val: u64) -> u64 {
        let g = CpuGuard::new();
        unsafe { AtomicU64::from_ptr(this_cpu_ptr(pcp, &g)) }.swap(val, Ordering::Relaxed)
    }

    #[inline(always)]
    pub(super) unsafe fn this_cpu_cmpxchg(pcp: *mut u64, old: u64, new: u64) -> u64 {
        let g = CpuGuard::new();
        match unsafe { AtomicU64::from_ptr(this_cpu_ptr(pcp, &g)) }
            .compare_exchange(old, new, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(v) | Err(v) => v,
        }
    }
}

#[cfg(not(all(kernel, any(target_arch = "x86_64", target_arch = "aarch64"))))]
mod arch {
    use super::{bch2_this_cpu_add_u64, bch2_this_cpu_cmpxchg_u64, bch2_this_cpu_ptr,
                bch2_this_cpu_xchg_u64, CpuGuard};

    pub(super) unsafe fn this_cpu_add(pcp: *mut u64, val: u64) {
        unsafe { bch2_this_cpu_add_u64(pcp, val) }
    }

    pub(super) unsafe fn this_cpu_ptr<T>(pcp: *mut T, _g: &CpuGuard) -> *mut T {
        unsafe { bch2_this_cpu_ptr(pcp.cast()).cast() }
    }

    pub(super) unsafe fn this_cpu_xchg(pcp: *mut u64, val: u64) -> u64 {
        unsafe { bch2_this_cpu_xchg_u64(pcp, val) }
    }

    pub(super) unsafe fn this_cpu_cmpxchg(pcp: *mut u64, old: u64, new: u64) -> u64 {
        unsafe { bch2_this_cpu_cmpxchg_u64(pcp, old, new) }
    }
}

use arch::{this_cpu_add, this_cpu_cmpxchg, this_cpu_ptr, this_cpu_xchg};

/// Preemption off while it lives. Not Send or Sync: it's this CPU's.
pub struct CpuGuard(PhantomData<*const ()>);

impl CpuGuard {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        unsafe { bch2_preempt_disable() };
        CpuGuard(PhantomData)
    }
}

impl Drop for CpuGuard {
    fn drop(&mut self) {
        unsafe { bch2_preempt_enable() };
    }
}

fn possible_cpus() -> impl Iterator<Item = c_uint> {
    let mut cpu: c_int = -1;
    core::iter::from_fn(move || {
        cpu = unsafe { bch2_cpu_possible_next(cpu) };
        (cpu >= 0).then_some(cpu as c_uint)
    })
}

// ── PerCpu, and its copies ───────────────────────────────────────────────

/// C's `T __percpu *`: see the top of the file.
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

    pub fn this_cpu<'g>(&'g self, g: &'g CpuGuard) -> ThisCpu<'g, T> {
        self.get().this_cpu(g)
    }
}

impl<T, const N: usize> PerCpu<[T; N]> {
    pub fn at(&self, i: usize) -> PerCpuRef<'_, T> {
        self.get().at(i)
    }
}

/// How far into a @T the field @f projects to is: in a T on the stack, never
/// read.
fn field_offset<T, U>(f: impl FnOnce(*const T) -> *const U) -> usize {
    let t = MaybeUninit::<T>::uninit();
    f(t.as_ptr()) as usize - t.as_ptr() as usize
}

/// A field of a PerCpuRef or ThisCpu: percpu_field!(c.capacity.pcpu.get(),
/// online_reserved[slot]).
#[macro_export]
macro_rules! percpu_field {
    ($p:expr, $($f:tt)+) => {{
        let p = $p;
        // SAFETY: a field of *t, projected without reading it
        unsafe { p.field(|t| ::core::ptr::addr_of!((*t).$($f)+)) }
    }};
}

impl<'a, T> PerCpuRef<'a, T> {
    /// percpu_field!()'s.
    ///
    /// # Safety
    /// @f only projects @t to a field of it, reading nothing.
    pub unsafe fn field<U>(self, f: impl FnOnce(*const T) -> *const U) -> PerCpuRef<'a, U> {
        PerCpuRef(self.0.wrapping_byte_add(field_offset(f)).cast(), PhantomData)
    }

    pub fn this_cpu<'g>(self, g: &'g CpuGuard) -> ThisCpu<'g, T>
        where 'a: 'g
    {
        ThisCpu(unsafe { this_cpu_ptr(self.0, g) }, PhantomData)
    }
}

impl<'a, T, const N: usize> PerCpuRef<'a, [T; N]> {
    pub fn at(self, i: usize) -> PerCpuRef<'a, T> {
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
    fn read(&self, cpu: c_uint) -> T {
        unsafe { core::ptr::read_volatile(bch2_per_cpu_ptr(self.0.cast(), cpu).cast::<T>()) }
    }

    /// Exact only with the adders excluded.
    pub fn sum(&self) -> T {
        let mut cpus = possible_cpus();
        let first = cpus.next().expect("CPU 0 is always possible");
        cpus.fold(self.read(first), |acc, cpu| acc.wrapping_add(self.read(cpu)))
    }

    /// For a read that races the adds.
    pub fn sum_nonnegative(&self) -> T {
        self.sum().clamp_nonnegative()
    }
}

impl PerCpuRef<'_, u64> {
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
        for cpu in possible_cpus() {
            // SAFETY: a live CPU's copy; another CPU's, written as read
            unsafe { core::ptr::write_volatile(bch2_per_cpu_ptr(self.0.cast(), cpu).cast::<u64>(), 0) };
        }
        self.xchg(v);
    }

    #[inline(always)]
    pub fn xchg(&self, v: u64) -> u64 {
        unsafe { this_cpu_xchg(self.0, v) }
    }

    /// Err(what it was), if it wasn't @old.
    #[inline(always)]
    pub fn cmpxchg(&self, old: u64, new: u64) -> Result<u64, u64> {
        match unsafe { this_cpu_cmpxchg(self.0, old, new) } {
            v if v == old => Ok(v),
            v             => Err(v),
        }
    }
}

/// This CPU's copy, as a place: see the top of the file.
pub struct ThisCpu<'g, T>(*mut T, PhantomData<&'g CpuGuard>);

impl<T> Clone for ThisCpu<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for ThisCpu<'_, T> {}

impl<'g, T> ThisCpu<'g, T> {
    /// percpu_field!()'s.
    ///
    /// # Safety
    /// @f only projects @t to a field of it, reading nothing.
    pub unsafe fn field<U>(self, f: impl FnOnce(*const T) -> *const U) -> ThisCpu<'g, U> {
        ThisCpu(self.0.wrapping_byte_add(field_offset(f)).cast(), PhantomData)
    }
}

impl<'g, T, const N: usize> ThisCpu<'g, [T; N]> {
    pub fn at(self, i: usize) -> ThisCpu<'g, T> {
        assert!(i < N);
        ThisCpu(self.0.cast::<T>().wrapping_add(i), PhantomData)
    }
}

impl<T: Copy> ThisCpu<'_, T> {
    pub fn get(&self) -> T {
        unsafe { core::ptr::read_volatile(self.0) }
    }

    pub fn set(&self, v: T) {
        unsafe { core::ptr::write_volatile(self.0, v) }
    }

    /// As a place, to read-modify-write: `*x.place() += 1`.
    pub fn place(&self) -> FakePlaceRef<'_, Self> {
        FakePlaceWrite::place(self)
    }
}

impl<T: Counters> ThisCpu<'_, T> {
    /// Not atomic as a whole: a load and a store of the struct.
    pub fn add(&self, delta: T) {
        self.set(self.get().wrapping_add(delta))
    }
}

impl<T: Copy> FakePlaceRead for ThisCpu<'_, T> {
    type Value = T;

    fn get(&self) -> T {
        ThisCpu::get(self)
    }
}

impl<T: Copy> FakePlaceWrite for ThisCpu<'_, T> {
    fn set(&self, v: T) {
        ThisCpu::set(self, v)
    }
}

// ── Counters ─────────────────────────────────────────────────────────────

/// How a counter, or a struct or array of them, sums: see the top of the
/// file. A struct gets it with #[fieldwise(Counters::wrapping_add(rhs),
/// Counters::clamp_nonnegative)].
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
