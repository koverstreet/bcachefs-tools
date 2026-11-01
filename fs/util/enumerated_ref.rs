// SPDX-License-Identifier: GPL-2.0

//! EnumeratedRef: struct enumerated_ref as a Refcount - a count that's
//! stopped and waited out, as a percpu_ref is, its refs counted by kind
//! when debugging. c->writes is one, and ca->io_ref[] two.
//!
//! The kinds are a debugging aid, for C's: every kind list has rust first,
//! so all of Rust's refs are kind 0.

#![allow(non_camel_case_types)]

use crate::c;
use crate::errcode::{ret_to_result_void, BchError};
use crate::util::refcount::{Refcount, TryRefcount};
use core::ffi::{c_int, c_uint};
use crate::util::Printbuf;
use crate::types::c_default;
use cstruct_macros::{c_extern, CStruct};

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct enumerated_ref {
    /* C's ENUMERATED_REF_DEBUG, which bcachefs.h defines from this: rustc
     * sees only the configuration, not what a header derives from it */
    #[cfg(CONFIG_BCACHEFS_DEBUG)]
    pub nr: core::ffi::c_uint,
    #[cfg(CONFIG_BCACHEFS_DEBUG)]
    pub dying: bool,
    #[cfg(CONFIG_BCACHEFS_DEBUG)]
    pub refs: *mut core::sync::atomic::AtomicIsize,
    #[cfg(not(CONFIG_BCACHEFS_DEBUG))]
    pub ref_: c::percpu_ref,
    pub stop_fn: Option<unsafe extern "C" fn(*mut c::enumerated_ref)>,
    pub stop_complete: c::completion,
}
c_default!(enumerated_ref);

c_extern! {
    pub fn rust_enumerated_ref_get(r: *mut c::enumerated_ref);
    pub fn rust___enumerated_ref_tryget(r: *mut c::enumerated_ref) -> bool;
    pub fn rust_enumerated_ref_tryget(r: *mut c::enumerated_ref) -> bool;
    pub fn rust_enumerated_ref_put(r: *mut c::enumerated_ref);
    pub fn rust_enumerated_ref_is_zero(r: *mut c::enumerated_ref) -> bool;
    pub fn enumerated_ref_start(r: *mut c::enumerated_ref);
    pub fn enumerated_ref_stop_async(r: *mut c::enumerated_ref);
    pub fn enumerated_ref_exit(r: *mut c::enumerated_ref);
    pub fn enumerated_ref_init(r: *mut c::enumerated_ref, nr: c_uint,
                               stop_fn: Option<unsafe extern "C" fn(*mut c::enumerated_ref)>) -> c_int;
    pub fn enumerated_ref_to_text(out: *mut Printbuf, r: *mut c::enumerated_ref,
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

const _: () = assert!(c::bch_write_ref::BCH_WRITE_REF_rust as u32 == 0);
const _: () = assert!(c::bch_dev_read_ref::BCH_DEV_READ_REF_rust as u32 == 0);
const _: () = assert!(c::bch_dev_write_ref::BCH_DEV_WRITE_REF_rust as u32 == 0);

/// Zeroed with what it's in, for C's enumerated_ref_init().
#[repr(transparent)]
pub struct EnumeratedRef {
    r: core::cell::UnsafeCell<c::enumerated_ref>,
}

// SAFETY: an enumerated_ref is got and put from any thread
unsafe impl Send for EnumeratedRef {}
unsafe impl Sync for EnumeratedRef {}

// SAFETY: all zeroes is an enumerated_ref before enumerated_ref_init()
unsafe impl ::pin_init::Zeroable for EnumeratedRef {}

impl EnumeratedRef {
    /// No refs held: stopped, or never started.
    pub fn is_zero(&self) -> bool {
        unsafe { c::rust_enumerated_ref_is_zero(self.r.get()) }
    }

    pub fn start(&self) {
        unsafe { enumerated_ref_start(self.r.get()) }
    }

    /// Trygets fail from now; the stop function's called when the last ref's
    /// put.
    pub fn stop_async(&self) {
        unsafe { enumerated_ref_stop_async(self.r.get()) }
    }

    /// @nr kinds of ref, @stop_fn called when the last is put after stopping.
    ///
    /// # Safety
    /// Once, before anything takes a ref.
    pub unsafe fn init(&self, nr: u32, stop_fn: unsafe extern "C" fn(*mut c::enumerated_ref))
        -> Result<(), BchError>
    {
        ret_to_result_void(unsafe { enumerated_ref_init(self.r.get(), nr, Some(stop_fn)) })
    }

    /// # Safety
    /// Nothing uses it after.
    pub unsafe fn exit(&self) {
        unsafe { enumerated_ref_exit(self.r.get()) }
    }

    /// The refs held, by kind - @names the kinds', NULL terminated.
    ///
    /// # Safety
    /// @names has an entry for each of this ref's kinds.
    pub unsafe fn to_text(&self, out: &mut Printbuf, names: *const *const crate::util::ffi::c_char) {
        unsafe { (*self.r.get()).to_text(out, names) }
    }
}

// SAFETY: C's get and put, of kind 0 - first in every kind list, so the
// debug build's count of it, refs[0], is always there. What an
// enumerated_ref is in is freed only once it's been stopped, which waits out
// every ref: the filesystem going read-only for c->writes, a device going
// offline for its io refs - bch2_dev_free() warns if one's still held
unsafe impl Refcount for EnumeratedRef {
    unsafe fn inc(&self) {
        unsafe { c::rust_enumerated_ref_get(self.r.get()) }
    }

    unsafe fn dec(&self) {
        unsafe { c::rust_enumerated_ref_put(self.r.get()) }
    }
}

// SAFETY: an enumerated_ref's trygets take a ref when they succeed
unsafe impl TryRefcount for EnumeratedRef {
    fn try_inc(&self) -> bool {
        unsafe { c::rust___enumerated_ref_tryget(self.r.get()) }
    }

    fn try_inc_live(&self) -> bool {
        unsafe { c::rust_enumerated_ref_tryget(self.r.get()) }
    }
}
