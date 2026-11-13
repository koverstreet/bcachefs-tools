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
