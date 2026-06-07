// SPDX-License-Identifier: GPL-2.0
//! Raw bindings for the userspace Linux kernel-compat layer — the in-tree
//! `include/` shim that bcachefs's fs/ code is built against in userspace.
//!
//! This is the userspace stand-in for the in-kernel `kernel` crate: the fs/
//! bindings (the `bcachefs-kernel` crate) own only bcachefs's own types, and
//! import everything else — `bio`, the `__le*`/`__u*` primitives, locks,
//! lists — from here. In a real kernel build those come from the kernel
//! itself, and this crate is swapped out behind a cfg.

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
