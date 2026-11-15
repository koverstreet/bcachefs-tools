// SPDX-License-Identifier: GPL-2.0

//! The kernel's struct posix_acl.

use core::ffi::c_int;
use core::mem::ManuallyDrop;
use core::ptr::NonNull;

use crate::c;

#[path = "../posix_acl.rs"]
mod common;
pub use common::*;

impl AclType {
    /// Anything else is a bug.
    pub(crate) fn from_vfs(type_: c_int) -> Self {
        match type_ {
            t if t == Self::Access as c_int  => Self::Access,
            t if t == Self::Default as c_int => Self::Default,
            _ => panic!("acl type {type_}"),
        }
    }
}

/// A struct posix_acl we hold a reference on, released when dropped. One
/// the caller holds is borrowed - borrow_raw() - and one handed to C, to
/// keep, goes with into_raw().
pub struct PosixAcl(NonNull<c::posix_acl>);

impl PosixAcl {
    /// @acl, its reference ours.
    ///
    /// # Safety
    /// @acl is a live posix_acl, and the caller gives up a reference on it.
    pub unsafe fn from_raw(acl: NonNull<c::posix_acl>) -> Self {
        PosixAcl(acl)
    }

    /// @acl, which the caller holds a reference on - None for NULL.
    ///
    /// # Safety
    /// @acl is NULL or a live posix_acl, for as long as this is used.
    pub unsafe fn borrow_raw(acl: *mut c::posix_acl) -> Option<ManuallyDrop<PosixAcl>> {
        NonNull::new(acl).map(|acl| ManuallyDrop::new(PosixAcl(acl)))
    }

    /// Another borrow of what this one holds.
    pub(crate) fn borrowed(&self) -> ManuallyDrop<PosixAcl> {
        ManuallyDrop::new(PosixAcl(self.0))
    }

    /// Our reference, for C to keep.
    pub fn into_raw(self) -> *mut c::posix_acl {
        ManuallyDrop::new(self).0.as_ptr()
    }

    pub(crate) fn as_raw(&self) -> *mut c::posix_acl {
        self.0.as_ptr()
    }

    pub(crate) fn count(&self) -> usize {
        // SAFETY: the header, the struct_group the fields are in
        unsafe { (*self.as_raw()).__bindgen_anon_1.hdr.as_ref().a_count as usize }
    }
}

/// Another reference: posix_acl_dup().
impl Clone for PosixAcl {
    fn clone(&self) -> Self {
        PosixAcl(NonNull::new(unsafe { c::posix_acl_dup(self.as_raw()) })
                 .expect("posix_acl_dup() of an ACL"))
    }
}

impl Drop for PosixAcl {
    fn drop(&mut self) {
        unsafe { c::posix_acl_release(self.as_raw()) }
    }
}
