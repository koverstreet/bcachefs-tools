// SPDX-License-Identifier: GPL-2.0

//! The data types of util/enumerated_ref_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::{c_extern, CStruct};


#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct enumerated_ref {
    /* C's ENUMERATED_REF_DEBUG, which types.rs defines from this: rustc
     * sees only the configuration, not what a header derives from it */
    #[cfg(CONFIG_BCACHEFS_DEBUG)]
    pub nr: core::ffi::c_uint,
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

// What Rust calls of util/enumerated_ref.h: C gets these as prototypes, in
// util/enumerated_ref_gen.h - see c_extern! in types/cstruct.rs.
c_extern! {
    pub fn rust_enumerated_ref_tryget(r: *mut c::enumerated_ref, idx: core::ffi::c_uint) -> bool;
    pub fn rust_enumerated_ref_put(r: *mut c::enumerated_ref, idx: core::ffi::c_uint);
}
