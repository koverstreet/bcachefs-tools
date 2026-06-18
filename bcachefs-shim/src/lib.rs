// SPDX-License-Identifier: GPL-2.0
//! Raw bindings for the userspace Linux kernel-compat layer — the in-tree
//! `include/` shim that bcachefs's fs/ code is built against in userspace.
//!
//! This is the userspace stand-in for the in-kernel `kernel` crate: the fs/
//! bindings (the `bcachefs-kernel` crate) own only bcachefs's own types, and
//! import everything else — `bio`, the `__le*`/`__u*` primitives, locks,
//! lists — from here. In a real kernel build those come from the kernel
//! itself, and this crate is swapped out behind a cfg.

/// Userspace stand-in for the in-kernel `#[pin_data]` attribute (a no-op — see
/// `bcachefs-shim-macros`). fs/ code imports this under `#[cfg(not(kernel))]`
/// and `kernel`'s real one otherwise.
pub use bcachefs_shim_macros::pin_data;

pub mod c {
    #![allow(
        non_camel_case_types,
        non_upper_case_globals,
        non_snake_case,
        dead_code,
        improper_ctypes,
        unnecessary_transmutes
    )]
    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
}

pub mod print {
    use crate::c;
    use std::ffi::CString;
    use std::fmt;

    pub fn pr_info(args: fmt::Arguments<'_>) {
        let Ok(msg) = CString::new(args.to_string()) else {
            return;
        };

        unsafe {
            c::printk(b"%s\0".as_ptr().cast(), msg.as_ptr());
        }
    }
}

#[macro_export]
macro_rules! pr_info {
    ($($arg:tt)*) => {
        $crate::print::pr_info(::std::format_args!($($arg)*))
    };
}

pub mod workqueue;
