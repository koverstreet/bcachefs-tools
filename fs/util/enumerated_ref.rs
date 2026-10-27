// SPDX-License-Identifier: GPL-2.0

//! EnumeratedRef: struct enumerated_ref as a Refcount - a count that's
//! stopped and waited out, as a percpu_ref is, its refs counted by kind
//! when debugging. c->writes is one, and ca->io_ref[] two.
//!
//! The kinds are a debugging aid, for C's: every kind list has rust first,
//! so all of Rust's refs are kind 0.

use crate::c;
use crate::util::refcount::{KeepsAlive, Refcount, TryRefcount};
use crate::util::Printbuf;

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

    /// The refs held, by kind - @names the kinds', NULL terminated.
    ///
    /// # Safety
    /// @names has an entry for each of this ref's kinds.
    pub unsafe fn to_text(&self, out: &mut Printbuf, names: *const *const crate::util::ffi::c_char) {
        unsafe { (*self.r.get()).to_text(out, names) }
    }
}

// SAFETY: C's get and put, of kind 0 - first in every kind list, so the
// debug build's count of it, refs[0], is always there
unsafe impl Refcount for EnumeratedRef {
    fn inc(&self) {
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

// SAFETY: what an enumerated_ref is in is freed only once it's been stopped,
// which waits out every ref: the filesystem going read-only for c->writes,
// a device going offline for its io refs - bch2_dev_free() warns if one's
// still held
unsafe impl KeepsAlive for EnumeratedRef {}
