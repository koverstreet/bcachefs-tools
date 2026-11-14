// SPDX-License-Identifier: GPL-2.0
//! Raw bindings for the userspace Linux kernel-compat layer — the in-tree
//! `include/` shim that bcachefs's fs/ code is built against in userspace.
//!
//! This is the userspace stand-in for the in-kernel `kernel` crate: the fs/
//! bindings (the `bcachefs-kernel` crate) own only bcachefs's own types, and
//! import everything else — `bio`, the `__le*`/`__u*` primitives, locks,
//! lists — from here. In a real kernel build those come from the kernel
//! itself, and this crate is swapped out behind a cfg.

pub mod alloc;
mod compress;
pub mod sync;

pub mod c {
    #![allow(
        non_camel_case_types,
        non_upper_case_globals,
        non_snake_case,
        dead_code,
        unknown_lints, // unnecessary_transmutes is rustc 1.88's
        unnecessary_transmutes,
        unsafe_op_in_unsafe_fn, // bindgen's array and union field helpers, at 0.72
        clippy::all // bindgen's output
    )]
    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

    // As the kernel's bindings have it - bindgen doesn't
    // (rust-lang/rust-bindgen#3196) - so a struct with bitfields can be
    // MaybeZeroable. Only this one: what's Zeroable here should be in-kernel.
    //
    // SAFETY: a newtype around its storage.
    unsafe impl<Storage> ::pin_init::Zeroable for __BindgenBitfieldUnit<Storage>
        where Storage: ::pin_init::Zeroable {}

    // The kernel's name: userspace's crc32c() is bch_crc32c(), so as not to
    // clash with libblkid's in a static build (include/linux/crc32c.h).
    pub use bch_crc32c as crc32c;

    pub use crate::compress::*;
}

pub mod print {
    use std::fmt;

    // Userspace's printk drops the loglevel: every one goes to stderr
    pub fn printk(args: fmt::Arguments<'_>) {
        eprint!("{args}");
    }
}

#[macro_export]
macro_rules! pr_info {
    ($($arg:tt)*) => {
        $crate::print::printk(::std::format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! pr_err {
    ($($arg:tt)*) => {
        $crate::print::printk(::std::format_args!($($arg)*))
    };
}
