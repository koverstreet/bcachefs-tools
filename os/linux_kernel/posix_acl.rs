// SPDX-License-Identifier: GPL-2.0

//! The kernel's struct posix_acl.

use core::ffi::c_int;
use core::mem::ManuallyDrop;
use core::ptr::NonNull;

use crate::c;
use crate::errcode::{ret_to_result_void, BchError};
use crate::os::ops::{err_class_ptr, from_raw, into_raw};
use crate::os::vfs_inode::VfsInode;

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

    /// A new inode's in @dir, with @mode as they make it: its default
    /// ACL, inherited, and its access ACL - posix_acl_create(). None for
    /// either it doesn't get.
    pub fn create(dir: &VfsInode, mode: &mut c::umode_t)
        -> Result<(Option<PosixAcl>, Option<PosixAcl>), BchError>
    {
        let (mut default_acl, mut acl) = (core::ptr::null_mut(), core::ptr::null_mut());
        ret_to_result_void(unsafe { c::posix_acl_create(dir.as_ptr(), mode, &mut default_acl, &mut acl) })?;
        Ok((NonNull::new(default_acl).map(|p| unsafe { PosixAcl::from_raw(p) }),
            NonNull::new(acl).map(|p| unsafe { PosixAcl::from_raw(p) })))
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

from_raw! {
    AclType = c_int => |r| AclType::from_vfs(r);
    /// The VFS's reference, None for no ACL.
    Option<ManuallyDrop<PosixAcl>> = *mut c::posix_acl => |r| PosixAcl::borrow_raw(r);
}

into_raw! {
    /// NULL for no ACL; the ACL's reference ours to give.
    Result<Option<PosixAcl>, BchError> => *mut c::posix_acl:
        |r| r.map_or_else(err_class_ptr, |acl| acl.map_or(core::ptr::null_mut(), PosixAcl::into_raw));
}
