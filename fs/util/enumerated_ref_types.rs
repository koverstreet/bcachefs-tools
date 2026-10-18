// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::util::Printbuf;
use crate::types::c_default;
use core::ffi::c_uint;
use cstruct_macros::{c_extern, CStruct};

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct enumerated_ref {
    /* C's ENUMERATED_REF_DEBUG, which bcachefs.h defines from this: rustc
     * sees only the configuration, not what a header derives from it */
    #[cfg(CONFIG_BCACHEFS_DEBUG)]
    pub nr: c_uint,
    #[cfg(CONFIG_BCACHEFS_DEBUG)]
    pub dying: bool,
    #[cfg(CONFIG_BCACHEFS_DEBUG)]
    pub refs: *mut c::atomic_long_t,
    #[cfg(not(CONFIG_BCACHEFS_DEBUG))]
    pub ref_: c::percpu_ref,
    pub stop_fn: Option<unsafe extern "C" fn(*mut c::enumerated_ref)>,
    pub stop_complete: c::completion,
}
c_default!(enumerated_ref);

c_extern! {
    pub fn rust_enumerated_ref_tryget(r: *mut c::enumerated_ref, idx: c_uint) -> bool;
    pub fn rust_enumerated_ref_put(r: *mut c::enumerated_ref, idx: c_uint);
    pub fn rust_enumerated_ref_is_zero(r: *mut c::enumerated_ref) -> bool;
    pub fn enumerated_ref_to_text(out: *mut c::printbuf, r: *mut c::enumerated_ref,
                                  names: *const *const crate::util::ffi::c_char);
}

impl enumerated_ref {
    /// Each kind's count of refs held - @names the kinds', NULL terminated.
    ///
    /// # Safety
    /// @names has an entry for each of this ref's kinds.
    pub unsafe fn to_text(&self, out: &mut Printbuf,
                          names: *const *const crate::util::ffi::c_char) {
        unsafe { enumerated_ref_to_text(out.as_raw(), core::ptr::from_ref(self).cast_mut(), names) }
    }
}
