// SPDX-License-Identifier: GPL-2.0
//! Userspace option-string helpers. `bch_opt_strs` and its parse/free C API live
//! in the userspace `libbcachefs.h`, so they and these methods belong here.

use crate::c;

pub use bcachefs_kernel::opts::opt_id;

impl c::bch_opt_strs {
    /// Set a deferred option string by option id.
    ///
    /// The string is strdup'd into C heap memory so it can be freed by
    /// `bch2_opt_strs_free`.
    pub fn set(&mut self, id: c::bch_opt_id, val: &std::ffi::CStr) {
        unsafe {
            self.__bindgen_anon_1.by_id[id.0 as usize] = libc::strdup(val.as_ptr());
        }
    }

    /// Parse all option strings into a `bch_opts` struct.
    pub fn parse(&self) -> c::bch_opts {
        unsafe { c::bch2_parse_opts(*self) }
    }

    /// Free all strdup'd option strings.
    pub fn free(&mut self) {
        unsafe { c::bch2_opt_strs_free(self) }
    }
}
