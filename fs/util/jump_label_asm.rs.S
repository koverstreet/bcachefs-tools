/* SPDX-License-Identifier: GPL-2.0 */
/*
 * util/jump_label.rs's asm, from the headers of the kernel the module is built
 * against - ARCH_STATIC_BRANCH_ASM(): the NOP the kernel patches into a jump
 * when the key is enabled, and its __jump_table entry, as that kernel has
 * them. As percpu_asm.rs.S: run through the C preprocessor with the C build's
 * flags; what follows the cut is Rust, so it can't hold an apostrophe or a
 * line starting with a hash.
 */

#include <linux/jump_label.h>

// Cut here.

#if defined(CONFIG_JUMP_LABEL) && (defined(CONFIG_X86_64) || defined(CONFIG_ARM64))

macro_rules! static_branch_asm {
    () => { concat_literals!(ARCH_STATIC_BRANCH_ASM("{symb} + {off} + {branch}", "{l_yes}")) };
}

#endif
