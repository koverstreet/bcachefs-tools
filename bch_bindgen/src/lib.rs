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
	// re-export above.
	#[allow(
		non_camel_case_types,
		non_upper_case_globals,
		non_snake_case,
		dead_code,
		improper_ctypes,
		unnecessary_transmutes
	)]
	mod generated {
		use bcachefs_kernel::c::*;

		include!(concat!(env!("OUT_DIR"), "/non_fs.rs"));
	}
	pub use generated::*;

	// sysfs_read_or_html_dirlist is declared in include/linux/kobject.h (the
	// kernel-compat tree, bcachefs-shim's territory) but is really a tools HTTP
	// helper that takes a printbuf (an fs/ type). It slips through every crate's
	// origin filter - bcachefs-shim can't emit it (no printbuf), the fs/ and
	// tools crates blocklist include/ - so bind it by hand here. Ideally its C
	// declaration moves to c_src/.
	extern "C" {
		pub fn sysfs_read_or_html_dirlist(
			path: *const ::core::ffi::c_char,
			out: *mut printbuf,
		) -> ::core::ffi::c_int;
	}
}

pub mod data;
pub mod fs;
pub mod keyutils;
pub mod opts;
pub mod sb;

pub use bcachefs_kernel::accounting;
pub use bcachefs_kernel::opt_id;
