// SPDX-License-Identifier: GPL-2.0

//! Static branches: static_branch_unlikely!(), for Rust - ours, not the kernel
//! crate's. Its macro includes asm made during the kernel's own build,
//! rust/kernel/generated_arch_static_branch_asm.rs, which a kernel's build
//! tree for external modules needn't have: mainline's install-extmod-build
//! ships nothing of rust/, Ubuntu has it in a package of its own, nixpkgs
//! doesn't have it at all.
//!
//! As util/percpu.rs: on x86_64 and arm64, with CONFIG_JUMP_LABEL, it's
//! inline, as C's is - the asm is the kernel's own, ARCH_STATIC_BRANCH_ASM()
//! from the headers of the kernel being built against (jump_label_asm.rs.S,
//! preprocessed by Makefile.rust): a NOP the kernel patches into a jump to the
//! label when the key is enabled. Elsewhere, and in userspace - where a key
//! is a plain int - it's C's static_branch_unlikely(), out of line.

use crate::c;
use cstruct_macros::c_extern;

// C's static_branch_unlikely(), out of line, where Rust has no asm of its own.
c_extern! {
    pub fn bch2_static_branch_unlikely(key: *mut c::static_key_false) -> bool;
}

#[cfg(all(kernel, CONFIG_JUMP_LABEL, any(target_arch = "x86_64", target_arch = "aarch64")))]
include!(concat!(env!("OUT_DIR"), "/jump_label_asm.rs"));

#[cfg(all(kernel, CONFIG_JUMP_LABEL, any(target_arch = "x86_64", target_arch = "aarch64")))]
pub(crate) use static_branch_asm;

/// Whether the static key at field @field of @key, a static of type @keytyp
/// that C defines, is enabled - unlikely, so the branch costs a NOP while it
/// isn't: static_branch_unlikely(). As the kernel crate's macro takes it: a
/// symbol, as the asm's __jump_table entry has to name one.
///
/// # Safety
/// @key is a static C defines, its @field a struct static_key_false that only C
/// writes.
#[macro_export]
macro_rules! static_branch_unlikely {
    ($key:path, $keytyp:ty, $field:ident) => {{
        #[cfg(all(kernel, CONFIG_JUMP_LABEL, target_arch = "x86_64"))]
        let enabled = 'enabled: {
            ::core::arch::asm!(
                $crate::util::jump_label::static_branch_asm!(),
                l_yes = label { break 'enabled true; },
                symb = sym $key,
                off = const ::core::mem::offset_of!($keytyp, $field),
                branch = const 0,
                options(att_syntax),
            );
            false
        };

        #[cfg(all(kernel, CONFIG_JUMP_LABEL, target_arch = "aarch64"))]
        let enabled = 'enabled: {
            ::core::arch::asm!(
                $crate::util::jump_label::static_branch_asm!(),
                l_yes = label { break 'enabled true; },
                symb = sym $key,
                off = const ::core::mem::offset_of!($keytyp, $field),
                branch = const 0,
            );
            false
        };

        #[cfg(not(all(kernel, CONFIG_JUMP_LABEL, any(target_arch = "x86_64", target_arch = "aarch64"))))]
        let enabled = $crate::util::jump_label::bch2_static_branch_unlikely(
            ::core::ptr::addr_of!($key.$field).cast_mut());

        enabled
    }};
}
