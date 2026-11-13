// SPDX-License-Identifier: GPL-2.0
//! Userspace-only bcachefs bindings - the std-dependent wrappers and the
//! non-`fs/` C bindings that can't live in the no_std `bcachefs-kernel` crate
//! yet. `bch_bindgen::c` is a superset: it re-exports the core crate's `fs/`
//! bindings and adds the non-`fs/` userspace ones, so the tools reach all C
//! symbols through it. Meant to shrink as this code is ported to no_std.

pub mod c {
	#![allow(ambiguous_glob_reexports)]

	pub use bcachefs_kernel::c::*;

	// The bindgen lints apply only to the generated bindings, not the
	// re-export above. improper_ctypes is an error: bcachefs's types are
	// blocklisted here and resolve to bcachefs_kernel's by name, and one
	// that's unsized - a jset, a bch_sb - would be a fat pointer where C has
	// a thin one. Its sized instance, Head<T>, is C's.
	#[allow(
		non_camel_case_types,
		non_upper_case_globals,
		non_snake_case,
		dead_code,
		unknown_lints,	// unnecessary_transmutes is rustc 1.88's
		unnecessary_transmutes,
		unsafe_op_in_unsafe_fn,	// bindgen's array and union field helpers, at 0.72
		clippy::all	// bindgen's output
	)]
	#[deny(improper_ctypes)]
	mod generated {
		use bcachefs_kernel::c::*;

		// C's: the unsized vstructs bindgen's fns name, as C's structs,
		// Head<T>: an item beats the glob
		type bch_sb = bcachefs_kernel::util::vstructs::Head<bcachefs_kernel::c::bch_sb>;
		type printbuf = bcachefs_kernel::util::printbuf::Printbuf;
		type bch_fs = bcachefs_kernel::fs::Fs;

		include!(concat!(env!("OUT_DIR"), "/non_fs.rs"));
	}
	pub use generated::*;
}

pub mod data;
pub mod fs;
pub mod keyutils;
pub mod sb;

pub use bcachefs_kernel::accounting;
pub use bcachefs_kernel::OptId;
