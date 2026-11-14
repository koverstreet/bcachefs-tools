// SPDX-License-Identifier: GPL-2.0
//! Userspace-only bcachefs bindings - the std-dependent wrappers that can't
//! live in the no_std `bcachefs-kernel` crate yet, and keyutils'.
//! `bch_bindgen::c` is the core crate's bindings. Meant to shrink as this code
//! is ported to no_std.

pub mod c {
	pub use bcachefs_kernel::c::*;
}

pub mod data;
pub mod fs;
pub mod keyutils;
pub mod sb;

pub use bcachefs_kernel::accounting;
pub use bcachefs_kernel::OptId;
