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
//!   xchg or cmpxchg, and at most reset by another. A slot is a Volatile:
//!   every access a relaxed atomic, so another CPU's reset is a store like
//!   any other - each_cpu() reaches every CPU's - and one racing this CPU's
//!   read-modify-write may be lost, which is what a slot is for. A struct of
//!   slots is CpuShared: every part another CPU writes a Volatile, or
//!   Opaque - a local_lock_t, which its CPU's lockers change.
//!
//! The types:
//!
//! - PerCpu<T> is the field. alloc() - or C - allocates it at init, and it's
//!   freed as it drops, or by free(); between, it's live for every possible CPU,
//!   which every operation takes as given, as C's do. Not Copy: nothing holds
//!   it past its struct.
//! - PerCpuRef<'a, T> borrows it, or a part of it - at(i), percpu_field!() -
//!   as every CPU's copy: sum() over all of them; read(), add(), set(),
//!   xchg() and cmpxchg() on this CPU's, each one operation.
//! - CpuCopy<'a, T> is one CPU's, from cpus(), read volatile: to sum part of
//!   a T - percpu_field!() of each - in one sweep over the CPUs, where a sum
//!   of each part would be a sweep apiece and sum() reads all of T.
//! - ThisCpu<'g, T> is this CPU's copy, while a Pinned guard keeps it this
//!   CPU: a place, as util/projections.rs's are, or add() of a delta to a
//!   struct of counters; a slot's, its Volatile. Pinned is a CpuGuard -
//!   preemption off - an IrqGuard, interrupts off, or a LocalLockGuard,
//!   this CPU's local_lock_t held.
//!
//! What's atomic is C's contract and no more: a this_cpu operation is whole
//! against this CPU - its interrupts and preemption - not against another CPU
//! writing the same copy. arm64's happen to be, x86's aren't, so nothing may
//! lean on it. ThisCpu's read-modify-write is a load and a store, as C's
//! under preempt_disable() are: an interrupt adding to the same copy between
//! them is lost, so a counter interrupts add to goes through PerCpuRef::add().
//! And it's whole against this CPU's other tasks only as far as the guard
//! keeps them out: a CpuGuard keeps them all out.
//! Another CPU's copy is only read volatile, never through a reference: it
//! may be being written.
//!
//! But a T only its own CPU ever touches - a cache, under an IrqGuard - is
//! had by reference, as_mut(), unsafely: this CPU's while the guard keeps out
//! the rest of this CPU, and each CPU's, CpuCopy's, at teardown.
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
//! A PerCpuWord - u32 or u64 - has three: read, add, cmpxchg. xchg is a
//! cmpxchg loop over them, as x86's C is.
//!
//! - x86_64: one %gs-relative instruction - mov, add, cmpxchg - none LOCKed.
//! - arm64: this CPU's offset from tpidr, then a load, the kernel's add,
//!   LSE's ldadd or LL/SC, or util/atomic.rs's cas for cmpxchg. The add
//!   leaves preemption on, where C's disables it: a task moved between
//!   reading the offset and adding adds, atomically, to the CPU it left -
//!   right for a counter, which is only ever summed. The load likewise reads
//!   the CPU it left. cmpxchg disables it, as C's does.
//! - Elsewhere, C's, out of line.
//!
//! Each arch's are written once, for every width: the asm takes the
//! operand's register modifier, which sizes the instruction.
//!
//! So are current, the CPU's number, preempt_disable() and _enable(),
//! this_cpu_ptr() and per_cpu_ptr(): on x86 each one instruction on the
//! kernel's own per-CPU variables, %rip relative as 6.15+ addresses them -
//! enabling preemption calls out only to reschedule - and local_irq_save() and
//! _restore() are pushf/cli and sti: under PARAVIRT_XXL, paravirt patch sites
//! as C's are, which the kernel patches to those at load but under Xen PV.
//! Where the kernel's config makes one more than that (preempt debugging or
//! tracing; traced irqs, RT) it's C's call. arm64 has current (sp_el0),
//! this_cpu_ptr() and per_cpu_ptr() inline, and the CPU's number and the
//! preempt count from current's thread_info; its irq flags are C's, out of
//! line. The walk over the possible CPUs reads the possible mask, everywhere.

use core::ffi::c_uint;
use crate::util::ffi::c_ulong;
use core::marker::PhantomData;
use core::mem::{align_of, size_of, ManuallyDrop, MaybeUninit};
use core::ptr::NonNull;
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
use core::sync::atomic::{compiler_fence, Ordering::SeqCst};

use crate::c;
use crate::c::rust_local_lock_init;
use crate::locking::MemallocFlags;
use crate::util::alloc::{current_gfp, current_gfp_context_raw, AllocError, Flags};
use crate::util::ffi::Opaque;
use crate::util::vstructs::Plain;
use crate::util::projections::{FakePlaceRef, FakePlaceWrite, FakePlaceRead, Volatile};

/// Whether a six lock asked for per-CPU reader counts gets them.
pub const SIX_PERCPU_READERS: bool = true;

pub use crate::c::{
    free_percpu, bch2_this_cpu_add_u32, bch2_this_cpu_add_u64, bch2_this_cpu_cmpxchg_u32, bch2_this_cpu_cmpxchg_u64,
    bch2_this_cpu_ptr, bch2_per_cpu_ptr,
};

/// The task kept on its CPU, till migrate_enable().
pub fn migrate_disable() {
    unsafe { c::bch2_migrate_disable() }
}

pub fn migrate_enable() {
    unsafe { c::bch2_migrate_enable() }
}

/// The CPU the task's on: unless it's kept there, it may not be by the time
/// this is used.
pub fn raw_smp_processor_id() -> u32 {
    arch::raw_smp_processor_id()
}

// ── The operations, an arch's own ────────────────────────────────────────

/// A word per-CPU data is operated on in: u32 or u64. Its operations are
/// this CPU's copy's, each one instruction where the arch has one; xchg is
/// a cmpxchg loop over them, as C's is. Each is C's this_cpu_*() of a Self,
/// for a @pcp - the callers' to keep - to a per-CPU Self, allocated for
/// every possible CPU and live.
pub trait PerCpuWord: Copy + Eq {
    unsafe fn this_cpu_read(pcp: *mut Self) -> Self;
    unsafe fn this_cpu_add(pcp: *mut Self, val: Self);
    unsafe fn this_cpu_cmpxchg(pcp: *mut Self, old: Self, new: Self) -> Self;
}

/* the preempt count only the count: debug and the preemptoff tracer make
 * preempt_count_add() and _sub() calls */
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
const PREEMPT_INLINE: bool = !cfg!(any(CONFIG_DEBUG_PREEMPT, CONFIG_TRACE_PREEMPT_TOGGLE));

/// preempt_disable(): the count up, then a compiler barrier - as C's, over
/// the arch's count where it's inline.
#[inline(always)]
pub(crate) fn preempt_disable() {
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    {
        if PREEMPT_INLINE {
            if cfg!(CONFIG_PREEMPT_COUNT) {
                arch::preempt_count_inc();
            }
            return compiler_fence(SeqCst);
        }
    }
    unsafe { c::bch2_preempt_disable() }
}

/// preempt_enable(): a compiler barrier, the count down, and if that's the
/// last and a reschedule's wanted, the reschedule.
#[inline(always)]
pub(crate) fn preempt_enable() {
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    {
        if PREEMPT_INLINE {
            compiler_fence(SeqCst);
            if cfg!(CONFIG_PREEMPT_COUNT) && arch::preempt_count_dec_and_test() && cfg!(CONFIG_PREEMPTION) {
                unsafe { c::bch2_preempt_schedule() };
            }
            return;
        }
    }
    unsafe { c::bch2_preempt_enable() }
}

#[cfg(target_arch = "x86_64")]
mod arch {
    use super::{PerCpuWord, Pinned};
    use crate::util::ffi::c_ulong;

    include!(concat!(env!("OUT_DIR"), "/percpu_asm.rs"));

    /* local_irq_save() and _restore() only pushf/cli and sti, or their
     * paravirt patch sites - not traced - and local_lock_irqsave() only that:
     * not RT's sleeping lock, no lockdep */
    const IRQ_INLINE: bool = !cfg!(any(CONFIG_TRACE_IRQFLAGS, CONFIG_PREEMPT_RT, CONFIG_DEBUG_LOCK_ALLOC));

    macro_rules! per_cpu_word {
        ($($t:ty, $w:literal);*) => { $(
            impl PerCpuWord for $t {
                #[inline(always)]
                unsafe fn this_cpu_read(pcp: *mut $t) -> $t {
                    let val: $t;
                    unsafe {
                        core::arch::asm!(this_cpu_read_asm!($w), ptr = in(reg) pcp, val = out(reg) val,
                                         options(att_syntax, nostack, readonly, preserves_flags));
                    }
                    val
                }

                #[inline(always)]
                unsafe fn this_cpu_add(pcp: *mut $t, val: $t) {
                    unsafe {
                        core::arch::asm!(this_cpu_add_asm!($w), ptr = in(reg) pcp, val = in(reg) val,
                                         options(att_syntax, nostack));
                    }
                }

                #[inline(always)]
                unsafe fn this_cpu_cmpxchg(pcp: *mut $t, old: $t, new: $t) -> $t {
                    let prev: $t;
                    unsafe {
                        core::arch::asm!(this_cpu_cmpxchg_asm!($w), ptr = in(reg) pcp, new = in(reg) new,
                                         inout("rax") old => prev, options(att_syntax, nostack));
                    }
                    prev
                }
            }
        )* };
    }
    per_cpu_word!(u32, ":e"; u64, ""; usize, "");

    #[inline(always)]
    pub(super) unsafe fn this_cpu_ptr<T>(pcp: *mut T, _g: &impl Pinned) -> *mut T {
        let mut p = pcp as usize;
        unsafe {
            core::arch::asm!(this_cpu_ptr_asm!(), ptr = inout(reg) p,
                             options(att_syntax, nostack, readonly));
        }
        p as *mut T
    }

    #[inline(always)]
    pub(super) unsafe fn per_cpu_ptr<T>(pcp: *mut T, cpu: u32) -> *mut T {
        let off: usize;
        unsafe {
            core::arch::asm!(per_cpu_offset_asm!(), cpu = in(reg) cpu as usize, off = out(reg) off,
                             options(att_syntax, nostack, pure, readonly, preserves_flags));
        }
        pcp.wrapping_byte_add(off)
    }

    /// The per-CPU current_task, as get_current() reads it.
    #[inline(always)]
    pub(crate) fn current_task() -> *mut crate::c::task_struct {
        let task: *mut crate::c::task_struct;
        unsafe {
            core::arch::asm!(current_task_asm!(), val = out(reg) task,
                             options(att_syntax, nostack, pure, readonly, preserves_flags));
        }
        task
    }

    #[inline(always)]
    pub(super) fn raw_smp_processor_id() -> u32 {
        let cpu: u32;
        unsafe {
            core::arch::asm!(cpu_number_asm!(), val = out(reg) cpu,
                             options(att_syntax, nostack, readonly, preserves_flags));
        }
        cpu
    }

    #[inline(always)]
    pub(super) fn preempt_count_inc() {
        unsafe { core::arch::asm!(preempt_count_inc_asm!(), options(att_syntax, nostack)) };
    }

    #[inline(always)]
    pub(super) fn preempt_count_dec_and_test() -> bool {
        let zero: u8;
        unsafe {
            core::arch::asm!(preempt_count_dec_asm!(), "sete {r}", r = out(reg_byte) zero,
                             options(att_syntax, nostack));
        }
        zero != 0
    }

    /* none nomem: compiler barriers, as C's */
    #[cfg(not(CONFIG_PARAVIRT_XXL))]
    mod irq {
        use crate::util::ffi::c_ulong;

        #[inline(always)]
        pub(super) fn save_fl() -> c_ulong {
            let flags: c_ulong;
            unsafe { core::arch::asm!("pushf", "pop {f}", f = out(reg) flags, options(att_syntax)) };
            flags
        }

        #[inline(always)]
        pub(super) fn disable() {
            unsafe { core::arch::asm!("cli", options(att_syntax, nostack)) };
        }

        #[inline(always)]
        pub(super) fn enable() {
            unsafe { core::arch::asm!("sti", options(att_syntax, nostack)) };
        }
    }

    /*
     * The patch sites' call is to a callee that saves all but rax - and, with
     * ZERO_CALL_USED_REGS, the argument registers (PVOP_VCALLEE_CLOBBERS): those
     * are clobbered here whatever the config.
     */
    #[cfg(CONFIG_PARAVIRT_XXL)]
    mod irq {
        use crate::util::ffi::c_ulong;

        macro_rules! pv_irq_op {
            ($asm:ident, $off:expr) => {{
                let rax: c_ulong;
                unsafe {
                    core::arch::asm!($asm!(), off = const $off, out("rax") rax,
                                     out("rcx") _, out("rdx") _, out("rsi") _, out("rdi") _,
                                     options(att_syntax));
                }
                rax
            }};
        }

        #[inline(always)]
        pub(super) fn save_fl() -> c_ulong {
            pv_irq_op!(pv_save_fl_asm, crate::c::PV_IRQ_SAVE_FL)
        }

        #[inline(always)]
        pub(super) fn disable() {
            pv_irq_op!(pv_irq_disable_asm, crate::c::PV_IRQ_DISABLE);
        }

        #[inline(always)]
        pub(super) fn enable() {
            pv_irq_op!(pv_irq_enable_asm, crate::c::PV_IRQ_ENABLE);
        }
    }

    #[inline(always)]
    pub(crate) fn local_irq_save() -> c_ulong {
        if !IRQ_INLINE {
            return unsafe { crate::c::rust_local_irq_save() };
        }
        let flags = irq::save_fl();
        irq::disable();
        flags
    }

    #[inline(always)]
    pub(crate) fn local_irq_restore(flags: c_ulong) {
        const X86_EFLAGS_IF: c_ulong = 1 << 9;

        if !IRQ_INLINE {
            return unsafe { crate::c::rust_local_irq_restore(flags) };
        }
        if flags & X86_EFLAGS_IF != 0 {
            irq::enable();
        }
    }

    #[inline(always)]
    pub(super) unsafe fn local_lock_irqsave(pcp: *mut crate::c::local_lock_t) -> c_ulong {
        if IRQ_INLINE { local_irq_save() } else { unsafe { crate::c::rust_local_lock_irqsave(pcp) } }
    }

    #[inline(always)]
    pub(super) unsafe fn local_unlock_irqrestore(pcp: *mut crate::c::local_lock_t, flags: c_ulong) {
        if IRQ_INLINE {
            local_irq_restore(flags)
        } else {
            unsafe { crate::c::rust_local_unlock_irqrestore(pcp, flags) }
        }
    }
}

#[cfg(target_arch = "aarch64")]
mod arch {
    use super::{CpuGuard, PerCpuWord, Pinned};
    use crate::util::ffi::c_ulong;
    use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

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

    macro_rules! per_cpu_word {
        ($($t:ty, $a:ty, $w:literal);*) => { $(
            impl PerCpuWord for $t {
                #[inline(always)]
                unsafe fn this_cpu_read(pcp: *mut $t) -> $t {
                    unsafe { <$a>::from_ptr(my_cpu(pcp)) }.load(Ordering::Relaxed)
                }

                #[inline(always)]
                unsafe fn this_cpu_add(pcp: *mut $t, val: $t) {
                    unsafe {
                        core::arch::asm!(this_cpu_add_asm!($w), ptr = in(reg) my_cpu(pcp), val = in(reg) val,
                                         tmp = out(reg) _, retry = out(reg) _, options(nostack));
                    }
                }

                #[inline(always)]
                unsafe fn this_cpu_cmpxchg(pcp: *mut $t, old: $t, new: $t) -> $t {
                    let g = CpuGuard::new();
                    match unsafe { <$a>::from_ptr(this_cpu_ptr(pcp, &g)) }
                        .compare_exchange(old, new, Ordering::Relaxed, Ordering::Relaxed) {
                        Ok(v) | Err(v) => v,
                    }
                }
            }
        )* };
    }
    per_cpu_word!(u32, AtomicU32, ":w"; u64, AtomicU64, ""; usize, AtomicUsize, "");

    #[inline(always)]
    pub(super) unsafe fn this_cpu_ptr<T>(pcp: *mut T, _g: &impl Pinned) -> *mut T {
        my_cpu(pcp)
    }

    /// The task in sp_el0, as get_current() reads it.
    #[inline(always)]
    pub(crate) fn current_task() -> *mut crate::c::task_struct {
        let task: *mut crate::c::task_struct;
        unsafe {
            core::arch::asm!("mrs {}, sp_el0", out(reg) task, options(nomem, nostack, pure, preserves_flags));
        }
        task
    }

    /* current_thread_info(): the head of current's task_struct */
    #[inline(always)]
    fn thread_info() -> *mut crate::c::thread_info {
        unsafe { &raw mut (*current_task()).thread_info }
    }

    #[inline(always)]
    pub(super) unsafe fn per_cpu_ptr<T>(pcp: *mut T, cpu: u32) -> *mut T {
        let off = unsafe { *(&raw const crate::c::__per_cpu_offset).cast::<c_ulong>().add(cpu as usize) };
        pcp.wrapping_byte_add(off as usize)
    }

    #[inline(always)]
    pub(super) fn raw_smp_processor_id() -> u32 {
        unsafe { (*thread_info()).cpu }
    }

    /* The count is current's, in its thread_info: a plain read and write, as
     * an interrupt that changes it puts it back */
    #[inline(always)]
    fn preempt_count() -> *mut u32 {
        unsafe { &raw mut (*thread_info()).__bindgen_anon_1.preempt.count }
    }

    #[inline(always)]
    pub(super) fn preempt_count_inc() {
        let pc = preempt_count();
        unsafe { pc.write_volatile(pc.read_volatile().wrapping_add(1)) };
    }

    /* need_resched is the count's top half, inverted: the whole word, read
     * after the decrement, 0 */
    #[inline(always)]
    pub(super) fn preempt_count_dec_and_test() -> bool {
        let pc = preempt_count();
        unsafe { pc.write_volatile(pc.read_volatile().wrapping_sub(1)) };
        unsafe { (&raw const (*thread_info()).__bindgen_anon_1.preempt_count).read_volatile() == 0 }
    }

    pub(crate) use super::c_arch::{local_irq_restore, local_irq_save};
    pub(super) use super::c_arch::{local_lock_irqsave, local_unlock_irqrestore};
}

/// What's C's, out of line, on an arch that doesn't have its own.
#[cfg(not(target_arch = "x86_64"))]
mod c_arch {

    pub(super) unsafe fn per_cpu_ptr<T>(pcp: *mut T, cpu: u32) -> *mut T {
        unsafe { super::bch2_per_cpu_ptr(pcp.cast(), cpu).cast() }
    }

    pub(super) fn raw_smp_processor_id() -> u32 {
        unsafe { crate::c::bch2_raw_smp_processor_id() }
    }

    pub(crate) fn local_irq_save() -> crate::util::ffi::c_ulong {
        unsafe { crate::c::rust_local_irq_save() }
    }

    pub(crate) fn local_irq_restore(flags: crate::util::ffi::c_ulong) {
        unsafe { crate::c::rust_local_irq_restore(flags) }
    }

    pub(super) unsafe fn local_lock_irqsave(pcp: *mut crate::c::local_lock_t) -> crate::util::ffi::c_ulong {
        unsafe { crate::c::rust_local_lock_irqsave(pcp) }
    }

    pub(super) unsafe fn local_unlock_irqrestore(pcp: *mut crate::c::local_lock_t, flags: crate::util::ffi::c_ulong) {
        unsafe { crate::c::rust_local_unlock_irqrestore(pcp, flags) }
    }
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
mod arch {
    use super::{bch2_this_cpu_add_u32, bch2_this_cpu_add_u64, bch2_this_cpu_cmpxchg_u32,
                bch2_this_cpu_cmpxchg_u64, bch2_this_cpu_ptr, PerCpuWord, Pinned};

    macro_rules! per_cpu_word {
        ($($t:ty, $add:ident, $cmpxchg:ident);*) => { $(
            impl PerCpuWord for $t {
                unsafe fn this_cpu_read(pcp: *mut $t) -> $t {
                    unsafe { core::ptr::read_volatile(bch2_this_cpu_ptr(pcp.cast()).cast::<$t>()) }
                }

                unsafe fn this_cpu_add(pcp: *mut $t, val: $t) {
                    unsafe { $add(pcp.cast(), val as _) }
                }

                unsafe fn this_cpu_cmpxchg(pcp: *mut $t, old: $t, new: $t) -> $t {
                    unsafe { $cmpxchg(pcp.cast(), old as _, new as _) as $t }
                }
            }
        )* };
    }
    per_cpu_word!(u32, bch2_this_cpu_add_u32, bch2_this_cpu_cmpxchg_u32;
                  u64, bch2_this_cpu_add_u64, bch2_this_cpu_cmpxchg_u64);
    #[cfg(target_pointer_width = "64")]
    per_cpu_word!(usize, bch2_this_cpu_add_u64, bch2_this_cpu_cmpxchg_u64);
    #[cfg(target_pointer_width = "32")]
    per_cpu_word!(usize, bch2_this_cpu_add_u32, bch2_this_cpu_cmpxchg_u32);

    pub(super) unsafe fn this_cpu_ptr<T>(pcp: *mut T, _g: &impl Pinned) -> *mut T {
        unsafe { bch2_this_cpu_ptr(pcp.cast()).cast() }
    }

    pub(crate) fn current_task() -> *mut crate::c::task_struct {
        unsafe { crate::c::bch2_current() }
    }

    pub(crate) use super::c_arch::{local_irq_restore, local_irq_save};
    pub(super) use super::c_arch::{local_lock_irqsave, local_unlock_irqrestore, per_cpu_ptr, raw_smp_processor_id};
}

use arch::{local_lock_irqsave, local_unlock_irqrestore, per_cpu_ptr, this_cpu_ptr};
pub(crate) use arch::{current_task, local_irq_restore, local_irq_save};

/// The task held to this CPU while it lives: for this CPU's copy, which
/// is the copy of whatever CPU the task is on.
///
/// # Safety
/// While a value lives the task doesn't migrate. Not Send or Sync: it's
/// this CPU's.
pub unsafe trait Pinned {}

/// Preemption off while it lives, and a NORECLAIM scope. Not Send or Sync:
/// it's this CPU's.
pub struct CpuGuard {
    _noreclaim: MemallocFlags,
    _p:         PhantomData<*const ()>,
}

// SAFETY: preemption's off
unsafe impl Pinned for CpuGuard {}

impl CpuGuard {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        preempt_disable();
        CpuGuard { _noreclaim: MemallocFlags::noreclaim(), _p: PhantomData }
    }
}

impl Drop for CpuGuard {
    fn drop(&mut self) {
        preempt_enable();
    }
}

/// This CPU's interrupts off while it lives, as local_irq_save(), and a
/// NORECLAIM scope: for a slot interrupt handlers change too - a read bio's
/// completion - which a CpuGuard's ThisCpu would race. Not Send or Sync: it's
/// this CPU's.
pub struct IrqGuard {
    flags:      c_ulong,
    _noreclaim: MemallocFlags,
    _p:         PhantomData<*const ()>,
}

// SAFETY: interrupts off, so no preemption either
unsafe impl Pinned for IrqGuard {}

impl IrqGuard {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        let flags = local_irq_save();
        IrqGuard { flags, _noreclaim: MemallocFlags::noreclaim(), _p: PhantomData }
    }
}

impl Drop for IrqGuard {
    fn drop(&mut self) {
        local_irq_restore(self.flags)
    }
}

/// This CPU's copy of a per-CPU local_lock_t, held: local_lock_irqsave(),
/// from PerCpuRef::lock_irqsave(). It pins the task and keeps out this CPU's
/// interrupts and other takers: interrupts off - or on PREEMPT_RT, a sleeping
/// lock with migration off, so what it guards may take sleeping locks. A
/// ThisCpu under it is whole only for what the lock guards.
pub struct LocalLockGuard<'a> {
    pcp:        *mut c::local_lock_t,
    flags:      c_ulong,
    _noreclaim: MemallocFlags,
    _p:         PhantomData<(&'a c::local_lock_t, *const ())>,
}

// SAFETY: migration's off while it's held - by the lock on RT, interrupts
// off elsewhere
unsafe impl Pinned for LocalLockGuard<'_> {}

impl Drop for LocalLockGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: this CPU's, which it took
        unsafe { local_unlock_irqrestore(self.pcp, self.flags) }
    }
}

/// for_each_possible_cpu(): the possible mask's bits below nr_cpu_ids. The
/// mask is set at boot.
#[cfg(CONFIG_SMP)]
fn possible_cpus() -> impl Iterator<Item = u32> {
    const BITS: usize = c_ulong::BITS as usize;
    let nr = crate::os::kernel::nr_cpu_ids() as usize;
    let words = unsafe { &(*(&raw const c::__cpu_possible_mask)).bits };

    words[..nr.div_ceil(BITS)].iter().enumerate()
        .flat_map(|(i, &w)| {
            let mut w = w;
            core::iter::from_fn(move || {
                (w != 0).then(|| {
                    let b = w.trailing_zeros() as usize;
                    w &= w - 1;
                    i * BITS + b
                })
            })
        })
        .take_while(move |&cpu| cpu < nr)
        .map(|cpu| cpu as u32)
}

#[cfg(not(CONFIG_SMP))]
fn possible_cpus() -> impl Iterator<Item = u32> {
    0..1
}

// ── PerCpu, and its copies ───────────────────────────────────────────────

/// C's `T __percpu *`: see the top of the file.
#[repr(transparent)]
pub struct PerCpu<T>(*mut T);

// Null: not yet allocated, as C's init finds it.
unsafe impl<T> ::pin_init::Zeroable for PerCpu<T> {}

unsafe impl<T: Send> Sync for PerCpu<T> {}

impl<T> Drop for PerCpu<T> {
    fn drop(&mut self) {
        self.free()
    }
}

/// A PerCpu, or a part of one, for as long as it's borrowed.
pub struct PerCpuRef<'a, T>(*mut T, PhantomData<&'a PerCpu<T>>);

impl<T> Clone for PerCpuRef<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for PerCpuRef<'_, T> {}

impl<T: ::pin_init::Zeroable> PerCpu<T> {
    /// Every possible CPU's copy, zeroed: alloc_percpu().
    pub fn alloc() -> Result<Self, AllocError> {
        Self::alloc_gfp(Flags::empty())
    }

    /// alloc(), allocating with @gfp: alloc_percpu_gfp().
    pub fn alloc_gfp(gfp: Flags) -> Result<Self, AllocError> {
        // In the kernel, __alloc_percpu_gfp() is a macro - alloc_hooks() over
        // pcpu_alloc_noprof() - which kernel Rust calls directly, as its
        // allocator does krealloc_noprof()
        let gfp = current_gfp_context_raw(gfp.raw());
        let p = unsafe { c::pcpu_alloc_noprof(size_of::<T>(), align_of::<T>(), false, gfp) };
        if p.is_null() { Err(AllocError) } else { Ok(PerCpu(p.cast())) }
    }

    /// alloc(), as a field's initializer.
    pub fn init() -> impl ::pin_init::Init<Self, AllocError> {
        // SAFETY: the slot written, or nothing done
        unsafe {
            ::pin_init::init_from_closure(|slot: *mut Self| {
                slot.write(Self::alloc()?);
                Ok(())
            })
        }
    }
}


/// alloc_init()'s initializer, of @copy from zeroes, for CPU @cpu.
pub type InitCopy<T, C> = unsafe extern "C" fn(copy: *mut T, ctx: *mut C, cpu: c_uint);

impl<T> PerCpu<T> {
    /// Every possible CPU's copy, initialized by @init.
    ///
    /// # Safety
    /// @init only initializes the copy it's given, from zeroes; @ctx lives
    /// until the PerCpu is freed.
    pub unsafe fn alloc_init<C>(init: InitCopy<T, C>, ctx: *mut C) -> Result<Self, AllocError> {
        let p = PerCpu::<MaybeUninit<T>>::alloc()?.into_raw().cast::<T>();
        for cpu in possible_cpus() {
            unsafe { init(per_cpu_ptr(p.cast::<u8>(), cpu).cast(), ctx, cpu) };
        }
        Ok(PerCpu(p))
    }
}

/// @n u64 counters, percpu, of a length known only when they're allocated:
/// freed when dropped.
pub struct PerCpuU64s {
    p: NonNull<u64>,
    n: usize,
}

unsafe impl Sync for PerCpuU64s {}

impl PerCpuU64s {
    pub fn new(n: usize) -> Result<Self, AllocError> {
        let size = n * size_of::<u64>();
        let gfp = current_gfp();
        let p = unsafe { c::pcpu_alloc_noprof(size, align_of::<u64>(), false, gfp) };
        Ok(Self { p: NonNull::new(p.cast()).ok_or(AllocError)?, n })
    }

    pub fn len(&self) -> usize {
        self.n
    }

    pub fn iter(&self) -> impl Iterator<Item = PerCpuRef<'_, u64>> + '_ {
        (0..self.n).map(move |i| unsafe { PerCpuRef::from_raw(self.p.as_ptr().add(i)) })
    }

    /// Each counter set to @v's, zero past its end: through &mut, so no add
    /// races it.
    pub fn set(&mut self, v: &[u64]) {
        for (i, c) in self.iter().enumerate() {
            c.set(v.get(i).copied().unwrap_or(0));
        }
    }
}

impl Drop for PerCpuU64s {
    fn drop(&mut self) {
        unsafe { c::free_percpu(self.p.as_ptr().cast()) }
    }
}

impl<T> PerCpu<T> {
    /// Whether it's been allocated - and not freed.
    pub fn allocated(&self) -> bool {
        !self.0.is_null()
    }

    /// Its pointer, for a `T __percpu *` to own: freed with free_percpu().
    pub fn into_raw(self) -> *mut T {
        ManuallyDrop::new(self).0
    }

    /// free_percpu(): unallocated after, as before alloc(). Its copies aren't
    /// dropped.
    pub fn free(&mut self) {
        unsafe { c::free_percpu(self.0.cast()) };
        self.0 = core::ptr::null_mut();
    }

    pub fn get(&self) -> PerCpuRef<'_, T> {
        PerCpuRef(self.0, PhantomData)
    }

    pub fn this_cpu<'g>(&'g self, g: &'g impl Pinned) -> ThisCpu<'g, T> {
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
    /// A C `T __percpu *` that isn't a PerCpu: a table's entry's counters.
    ///
    /// # Safety
    /// @p is a percpu allocation of a @T, live for 'a.
    pub unsafe fn from_raw(p: *mut T) -> Self {
        PerCpuRef(p, PhantomData)
    }

    /// percpu_field!()'s.
    ///
    /// # Safety
    /// @f only projects @t to a field of it, reading nothing.
    pub unsafe fn field<U>(self, f: impl FnOnce(*const T) -> *const U) -> PerCpuRef<'a, U> {
        PerCpuRef(self.0.wrapping_byte_add(field_offset(f)).cast(), PhantomData)
    }

    /// Each possible CPU's copy.
    #[inline]
    pub fn cpus(self) -> impl Iterator<Item = CpuCopy<'a, T>> {
        possible_cpus().map(move |cpu| CpuCopy(unsafe { per_cpu_ptr(self.0, cpu) }, PhantomData))
    }

    pub fn this_cpu<'g>(self, g: &'g impl Pinned) -> ThisCpu<'g, T>
        where 'a: 'g
    {
        ThisCpu(unsafe { this_cpu_ptr(self.0, g) }, PhantomData)
    }

    /// Every CPU's copy zeroed: as set()'s, an add racing it may or may not
    /// count - and any bytes are a T, so what it races leaves one.
    pub fn zero(self) where T: Default + Plain {
        for cpu in self.cpus() {
            unsafe { cpu.0.cast_mut().write_volatile(T::default()) };
        }
    }
}

// SAFETY: a zeroed local_lock_t is a valid uninitialized one, as in C's
// alloc_percpu()'d struct; init() below initializes it
unsafe impl ::pin_init::Zeroable for Opaque<c::local_lock_t> {}

impl<'a> PerCpuRef<'a, Opaque<c::local_lock_t>> {
    /// Each CPU's initialized: local_lock_init(), before any takes it.
    pub fn init(self) {
        for cpu in self.cpus() {
            // SAFETY: a CPU's copy, which no one takes till it's initialized
            unsafe { rust_local_lock_init(cpu.0.cast_mut().cast()) }
        }
    }

    /// This CPU's, taken: local_lock_irqsave().
    pub fn lock_irqsave(self) -> LocalLockGuard<'a> {
        let pcp = self.0.cast::<c::local_lock_t>();
        // SAFETY: live for 'a, and initialized
        let flags = unsafe { local_lock_irqsave(pcp) };
        LocalLockGuard { pcp, flags, _noreclaim: MemallocFlags::noreclaim(), _p: PhantomData }
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
    /// Exact only with the adders excluded.
    pub fn sum(&self) -> T {
        let mut cpus = self.cpus().map(|c| c.get());
        let first = cpus.next().expect("CPU 0 is always possible");
        cpus.fold(first, T::wrapping_add)
    }

    /// For a read that races the adds.
    pub fn sum_nonnegative(&self) -> T {
        self.sum().clamp_nonnegative()
    }
}

impl<T: PerCpuWord + From<u8>> PerCpuRef<'_, T> {
    /// This CPU's, or the one the task was just on: for a cmpxchg() to start
    /// from, which fails unless it's this CPU's value now.
    #[inline(always)]
    pub fn read(&self) -> T {
        unsafe { T::this_cpu_read(self.0) }
    }

    /// A subtraction is the add of its wrapping_neg().
    #[inline(always)]
    pub fn add(&self, v: T) {
        unsafe { T::this_cpu_add(self.0, v) }
    }

    /// This CPU's to @v, every other's to 0. An add racing it may or may not
    /// count: whether that matters is up to what the variable is for - or
    /// there are no adders yet.
    pub fn set(&self, v: T) {
        for cpu in possible_cpus() {
            // SAFETY: a live CPU's copy; another CPU's, written as read
            unsafe { core::ptr::write_volatile(per_cpu_ptr(self.0, cpu), T::from(0)) };
        }
        self.xchg(v);
    }

    #[inline(always)]
    pub fn xchg(&self, v: T) -> T {
        let mut old = self.read();
        loop {
            match self.cmpxchg(old, v) {
                Ok(old) => return old,
                Err(cur) => old = cur,
            }
        }
    }

    /// Err(what it was), if it wasn't @old.
    #[inline(always)]
    pub fn cmpxchg(&self, old: T, new: T) -> Result<T, T> {
        match unsafe { T::this_cpu_cmpxchg(self.0, old, new) } {
            v if v == old => Ok(v),
            v             => Err(v),
        }
    }
}

/// A per-CPU T any CPU may hold a shared reference to: every part another
/// CPU writes atomic - a Volatile - or behind an UnsafeCell - Opaque.
///
/// # Safety
/// A &T races nothing another CPU does to its copy.
pub unsafe trait CpuShared {}

// SAFETY: every access atomic
unsafe impl<T> CpuShared for Volatile<T> {}
// SAFETY: each element's
unsafe impl<T: CpuShared, const N: usize> CpuShared for [T; N] {}
// SAFETY: an UnsafeCell's
unsafe impl<T> CpuShared for Opaque<T> {}

impl<'a, T: CpuShared> PerCpuRef<'a, T> {
    /// Each possible CPU's copy: slots, anyone's to load and store.
    pub fn each_cpu(self) -> impl Iterator<Item = &'a T> {
        // SAFETY: a live CPU's copy, for 'a; CpuShared, so a shared
        // reference races nothing
        possible_cpus().map(move |cpu| unsafe { &*per_cpu_ptr(self.0, cpu) })
    }

    /// This CPU's copy - or by the time it's used, the one this was on:
    /// raw_cpu_ptr(), for a copy any CPU may use.
    pub fn raw_this_cpu(self) -> &'a T {
        let g = CpuGuard::new();
        // SAFETY: as each_cpu()'s, of the CPU @g pinned us to
        unsafe { &*this_cpu_ptr(self.0, &g) }
    }
}

impl<'a> PerCpuRef<'a, Volatile<u64>> {
    /// The slot as a word: its read() and cmpxchg(), this CPU's.
    pub fn word(self) -> PerCpuRef<'a, u64> {
        // Volatile<u64> is laid out as a u64 (repr(transparent))
        PerCpuRef(self.0.cast(), PhantomData)
    }
}

/// One CPU's copy, from PerCpuRef::cpus(): see the top of the file.
pub struct CpuCopy<'a, T>(*const T, PhantomData<&'a PerCpu<T>>);

impl<T> Clone for CpuCopy<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for CpuCopy<'_, T> {}

impl<'a, T> CpuCopy<'a, T> {
    /// percpu_field!()'s.
    ///
    /// # Safety
    /// @f only projects @t to a field of it, reading nothing.
    #[inline]
    pub unsafe fn field<U>(self, f: impl FnOnce(*const T) -> *const U) -> CpuCopy<'a, U> {
        CpuCopy(self.0.wrapping_byte_add(field_offset(f)).cast(), PhantomData)
    }

    /// Its CPU's copy, as a pointer: for its owner to initialize, or tear
    /// down.
    pub fn as_ptr(self) -> *const T {
        self.0
    }

    /// Its CPU's copy, by reference: for its owner while no CPU uses it - its
    /// teardown.
    ///
    /// # Safety
    /// Nothing else reaches it while 'a lasts, here or on any CPU.
    pub unsafe fn as_mut(self) -> &'a mut T {
        unsafe { &mut *self.0.cast_mut() }
    }
}

impl<T: Copy> CpuCopy<'_, T> {
    #[inline]
    pub fn get(&self) -> T {
        // SAFETY: a live CPU's copy, written as it's read
        unsafe { core::ptr::read_volatile(self.0) }
    }
}

/// This CPU's copy, as a place: see the top of the file.
/// Not Send or Sync, as its guard isn't.
pub struct ThisCpu<'g, T>(*mut T, PhantomData<(&'g (), *const ())>);

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

    /// This CPU's copy, by reference: for a T only its own CPU touches, which
    /// the guard keeps the rest of this CPU out of - an IrqGuard, for one
    /// interrupt handlers change.
    ///
    /// # Safety
    /// Nothing else reaches it while 'g lasts: no other CPU, nothing the guard
    /// lets in on this one, and no other ThisCpu of it.
    pub unsafe fn as_mut(self) -> &'g mut T {
        unsafe { &mut *self.0 }
    }
}

impl<'g, T: CpuShared> core::ops::Deref for ThisCpu<'g, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: this CPU's copy, live while the guard is; CpuShared, so a
        // shared reference races nothing
        unsafe { &*self.0 }
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

macro_rules! counters {
    ($($t:ty, $signed:ty);*) => { $(
        impl Counters for $t {
            #[inline]
            fn wrapping_add(self, rhs: $t) -> $t {
                <$t>::wrapping_add(self, rhs)
            }

            #[inline]
            fn clamp_nonnegative(self) -> $t {
                (self as $signed).max(0) as $t
            }
        }
    )* };
}
counters!(u32, i32; u64, i64);

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
