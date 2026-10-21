/* SPDX-License-Identifier: GPL-2.0 */
/*
 * util/percpu.rs's asm, from the headers of the kernel the module is built
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

#if defined(CONFIG_X86_64) && defined(CONFIG_SMP)

macro_rules! this_cpu_add_asm {
    () => { concat_literals!("addq {val}, %" __stringify(__percpu_seg) ":({ptr})") };
}

macro_rules! this_cpu_cmpxchg_asm {
    () => { concat_literals!("cmpxchgq {new}, %" __stringify(__percpu_seg) ":({ptr})") };
}

macro_rules! this_cpu_xchg_asm {
    () => { concat_literals!("movq %" __stringify(__percpu_seg) ":({ptr}), %rax\n"
                             "2:\tcmpxchgq {new}, %" __stringify(__percpu_seg) ":({ptr})\n"
                             "\tjnz 2b") };
}

#elif defined(CONFIG_X86_64)

macro_rules! this_cpu_add_asm {
    () => { concat_literals!("addq {val}, ({ptr})") };
}

macro_rules! this_cpu_cmpxchg_asm {
    () => { concat_literals!("cmpxchgq {new}, ({ptr})") };
}

macro_rules! this_cpu_xchg_asm {
    () => { concat_literals!("movq ({ptr}), %rax\n"
                             "2:\tcmpxchgq {new}, ({ptr})\n"
                             "\tjnz 2b") };
}

#elif defined(CONFIG_ARM64)

macro_rules! my_cpu_offset_asm {
    () => { concat_literals!(ALTERNATIVE("mrs {off}, tpidr_el1",
                                         "mrs {off}, tpidr_el2",
                                         ARM64_HAS_VIRT_HOST_EXTN)) };
}

macro_rules! this_cpu_add_asm {
    () => { concat_literals!(ARM64_LSE_ATOMIC_INSN(
        "1:\tldxr\t{tmp}, [{ptr}]\n"
        "\tadd\t{tmp}, {tmp}, {val}\n"
        "\tstxr\t{retry:w}, {tmp}, [{ptr}]\n"
        "\tcbnz\t{retry:w}, 1b",
        "ldadd\t{val}, {tmp}, [{ptr}]\n"
        __nops(3))) };
}

#endif
