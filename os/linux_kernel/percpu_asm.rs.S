/* SPDX-License-Identifier: GPL-2.0 */
/*
 * percpu.rs's asm, from the headers of the kernel the module is built
 * against - what the kernel patches at boot, arm64's alternatives and their
 * cpucap numbers, is as that kernel has it. Run through the C preprocessor
 * with the C build's flags; what follows the cut is Rust, so it can't hold
 * an apostrophe or a line starting with a hash.
 */

#include <linux/percpu.h>
#ifdef CONFIG_ARM64
#include <asm/alternative-macros.h>
#include <asm/lse.h>
#endif

/*
 * 7.3+ x86-64 kernels with a preempt count keep it in a 64 bit word,
 * need_resched in the top bit: the decrement must see all of it.
 */
#ifdef CONFIG_HAS_SEPARATE_PREEMPT_RESCHED_BITS
#define PREEMPT_COUNT_OP(op)	op "q"
#else
#define PREEMPT_COUNT_OP(op)	op "l"
#endif

#ifdef CONFIG_PARAVIRT_XXL
#include <asm/paravirt_types.h>

/*
 * A paravirt irq op's call site, as ____PVOP_ALT_CALL() makes one: an indirect
 * call through the op's slot in pv_ops, at {off}, which the kernel patches at
 * load to a direct call - and, but under Xen PV, to @native. PARAVIRT_CALL,
 * but for its "m" operand, which Rust asm doesn't have.
 */
#define PV_IRQ_CALL(native)							\
	ALTERNATIVE_2(ANNOTATE_RETPOLINE_SAFE "\n\t" "call *pv_ops+{off}(%rip)",	\
		      ALT_CALL_INSTR, ALT_CALL_ALWAYS, native, ALT_NOT_XEN)
#endif

// Cut here.

// Each for every width: $w is the register modifier for the operand, which
// sizes the instruction - nothing for 64 bits, or 32 bits: :e on x86, :w on
// arm64.

#if defined(CONFIG_X86_64) && defined(CONFIG_SMP)

macro_rules! this_cpu_read_asm {
    ($w:literal) => { concat_literals!("mov %" __stringify(__percpu_seg) ":({ptr}), {val" $w "}") };
}

macro_rules! this_cpu_add_asm {
    ($w:literal) => { concat_literals!("add {val" $w "}, %" __stringify(__percpu_seg) ":({ptr})") };
}

macro_rules! this_cpu_cmpxchg_asm {
    ($w:literal) => { concat_literals!("cmpxchg {new" $w "}, %" __stringify(__percpu_seg) ":({ptr})") };
}

// The kernel's own per-CPU variables, %rip relative, as 6.15+ address them

macro_rules! current_task_asm {
    () => { concat_literals!("mov %" __stringify(__percpu_seg) ":current_task(%rip), {val}") };
}

macro_rules! this_cpu_ptr_asm {
    () => { concat_literals!("add %" __stringify(__percpu_seg) ":this_cpu_off(%rip), {ptr}") };
}

macro_rules! per_cpu_offset_asm {
    () => { concat_literals!("mov __per_cpu_offset(,{cpu},8), {off}") };
}

macro_rules! cpu_number_asm {
    () => { concat_literals!("mov %" __stringify(__percpu_seg) ":cpu_number(%rip), {val:e}") };
}

macro_rules! preempt_count_inc_asm {
    () => { concat_literals!(PREEMPT_COUNT_OP("inc") " %" __stringify(__percpu_seg) ":__preempt_count(%rip)") };
}

// sets ZF if the count, which holds need_resched inverted, is now 0
macro_rules! preempt_count_dec_asm {
    () => { concat_literals!(PREEMPT_COUNT_OP("dec") " %" __stringify(__percpu_seg) ":__preempt_count(%rip)") };
}

#elif defined(CONFIG_X86_64)

macro_rules! this_cpu_read_asm {
    ($w:literal) => { concat_literals!("mov ({ptr}), {val" $w "}") };
}

macro_rules! this_cpu_add_asm {
    ($w:literal) => { concat_literals!("add {val" $w "}, ({ptr})") };
}

macro_rules! this_cpu_cmpxchg_asm {
    ($w:literal) => { concat_literals!("cmpxchg {new" $w "}, ({ptr})") };
}

macro_rules! current_task_asm {
    () => { concat_literals!("mov current_task(%rip), {val}") };
}

macro_rules! this_cpu_ptr_asm {
    () => { concat_literals!("/* {ptr} */") };
}

macro_rules! per_cpu_offset_asm {
    () => { concat_literals!("xor {off:e}, {off:e} /* {cpu} */") };
}

macro_rules! cpu_number_asm {
    () => { concat_literals!("xor {val:e}, {val:e}") };
}

macro_rules! preempt_count_inc_asm {
    () => { concat_literals!(PREEMPT_COUNT_OP("inc") " __preempt_count(%rip)") };
}

macro_rules! preempt_count_dec_asm {
    () => { concat_literals!(PREEMPT_COUNT_OP("dec") " __preempt_count(%rip)") };
}

#elif defined(CONFIG_ARM64)

macro_rules! my_cpu_offset_asm {
    () => { concat_literals!(ALTERNATIVE("mrs {off}, tpidr_el1",
                                         "mrs {off}, tpidr_el2",
                                         ARM64_HAS_VIRT_HOST_EXTN)) };
}

macro_rules! this_cpu_add_asm {
    ($w:literal) => { concat_literals!(ARM64_LSE_ATOMIC_INSN(
        "1:\tldxr\t{tmp" $w "}, [{ptr}]\n"
        "\tadd\t{tmp" $w "}, {tmp" $w "}, {val" $w "}\n"
        "\tstxr\t{retry:w}, {tmp" $w "}, [{ptr}]\n"
        "\tcbnz\t{retry:w}, 1b",
        "ldadd\t{val" $w "}, {tmp" $w "}, [{ptr}]\n"
        __nops(3))) };
}

#endif

#if defined(CONFIG_X86_64) && defined(CONFIG_PARAVIRT_XXL)

// arch_local_save_flags(), _irq_disable() and _irq_enable(), paravirt

macro_rules! pv_save_fl_asm {
    () => { concat_literals!(PV_IRQ_CALL("pushf; pop %rax;")) };
}

macro_rules! pv_irq_disable_asm {
    () => { concat_literals!(PV_IRQ_CALL("cli;")) };
}

macro_rules! pv_irq_enable_asm {
    () => { concat_literals!(PV_IRQ_CALL("sti;")) };
}

#endif

// Whether there's a preempt count, and whether enabling preemption may
// reschedule: preempt_disable() and _enable() as that kernel's config has them

#ifdef CONFIG_X86_64

#ifdef CONFIG_PREEMPT_COUNT
const PREEMPT_COUNT: bool = true;
#else
const PREEMPT_COUNT: bool = false;
#endif

#ifdef CONFIG_PREEMPTION
const PREEMPTION: bool = true;
#else
const PREEMPTION: bool = false;
#endif

// Whether the count is only the count: debug and the preemptoff tracer make
// preempt_count_add() and _sub() calls

#if defined(CONFIG_DEBUG_PREEMPT) || defined(CONFIG_TRACE_PREEMPT_TOGGLE)
const PREEMPT_INLINE: bool = false;
#else
const PREEMPT_INLINE: bool = true;
#endif

// Whether local_irq_save() and _restore() are only pushf/cli and sti, or
// their paravirt patch sites - not traced - and local_lock_irqsave() is only
// that: not RT's sleeping lock, no lockdep

#if defined(CONFIG_TRACE_IRQFLAGS) || defined(CONFIG_PREEMPT_RT) || \
    defined(CONFIG_DEBUG_LOCK_ALLOC)
const IRQ_INLINE: bool = false;
#else
const IRQ_INLINE: bool = true;
#endif

#endif
