// SPDX-License-Identifier: GPL-2.0
//! Userspace option-string helpers. `bch_opt_strs` lives in the userspace
//! `libbcachefs.h`, so these methods belong here.

use crate::c;

pub use bcachefs_kernel::opts::opt_id;

impl c::bch_opt_strs {
    /// Set a deferred option string by option id.
    ///
    /// The string is strdup'd into C heap memory, which free() frees.
    pub fn set(&mut self, id: c::bch_opt_id, val: &std::ffi::CStr) {
        unsafe {
            self.__bindgen_anon_1.by_id[id.0 as usize] = libc::strdup(val.as_ptr());
        }
    }

    /// Free all strdup'd option strings.
    pub fn free(&mut self) {
        // SAFETY: each NULL or set()'s strdup()
        for s in unsafe { &mut self.__bindgen_anon_1.by_id } {
            unsafe { libc::free((*s).cast()) };
            *s = std::ptr::null_mut();
        }
    }
}
