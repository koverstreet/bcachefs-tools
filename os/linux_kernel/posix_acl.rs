// SPDX-License-Identifier: GPL-2.0

//! The kernel's struct posix_acl.

use core::mem::ManuallyDrop;
use core::ptr::NonNull;

use crate::c;
use crate::errcode::{ret_to_result_void, BchError};
use crate::os::ops::{err_class_ptr, from_raw, into_raw};
use crate::os::vfs_inode::VfsInode;
use crate::posix::mode::umode_t;
use crate::util::alloc::current_gfp;

#[path = "../posix_acl.rs"]
mod common;
pub use common::*;

impl AclType {
    /// Anything else is a bug.
    pub(crate) fn from_vfs(type_: i32) -> Self {
        match type_ {
            t if t == Self::Access as i32  => Self::Access,
            t if t == Self::Default as i32 => Self::Default,
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

    /// Its entries, as (tag, perm, id): the id in the initial user namespace,
    /// 0 for a tag that has none.
    pub(crate) fn entries(&self) -> impl Iterator<Item = (u16, u16, u32)> + '_ {
        // an ACL's entries follow it, a_count of them
        let entries = unsafe { (*self.as_raw()).a_entries.as_slice(self.count()) };
        entries.iter().map(|e| {
            let tag = e.e_tag as u16;
            // the union is the id the tag says, if it has one
            let id = unsafe {
                match tag as u32 {
                    c::ACL_USER  => c::from_kuid(&raw mut c::init_user_ns, *e.__bindgen_anon_1.e_uid.as_ref()),
                    c::ACL_GROUP => c::from_kgid(&raw mut c::init_user_ns, *e.__bindgen_anon_1.e_gid.as_ref()),
                    _            => 0,
                }
            };
            (tag, e.e_perm, id)
        })
    }

    /// A copy with its permissions changed to match file mode @mode:
    /// __posix_acl_chmod(), allocating with the current scope's flags.
    pub(crate) fn chmod(&self, mode: umode_t) -> Result<PosixAcl, BchError> {
        // __posix_acl_chmod() consumes the reference it's given, failing too
        let mut acl = self.clone().into_raw();
        let ret = ret_to_result_void(unsafe { c::__posix_acl_chmod(&mut acl, current_gfp(), mode.0) });
        let acl = NonNull::new(acl).map(PosixAcl);
        ret?;
        Ok(acl.expect("__posix_acl_chmod() succeeded without an ACL"))
    }

    /// A new inode's in @dir, with @mode as they make it: its default
    /// ACL, inherited, and its access ACL - posix_acl_create(). None for
    /// either it doesn't get.
    pub fn create<S>(dir: &VfsInode<S>, mode: &mut umode_t)
        -> Result<(Option<PosixAcl>, Option<PosixAcl>), BchError>
    {
        let (mut default_acl, mut acl) = (core::ptr::null_mut(), core::ptr::null_mut());
        ret_to_result_void(unsafe { c::posix_acl_create(dir.as_ptr(), &mut mode.0, &mut default_acl, &mut acl) })?;
        Ok((NonNull::new(default_acl).map(|p| unsafe { PosixAcl::from_raw(p) }),
            NonNull::new(acl).map(|p| unsafe { PosixAcl::from_raw(p) })))
    }
}

/// An ACL being filled in: its only reference ours, so its entries are ours
/// to write. A PosixAcl once it's done, to be shared.
pub struct NewPosixAcl(PosixAcl);

impl NewPosixAcl {
    /// @nr entries, to be set: posix_acl_alloc(), with the current scope's
    /// flags.
    pub(crate) fn alloc(nr: u32) -> Option<Self> {
        NonNull::new(unsafe { c::posix_acl_alloc(nr, current_gfp()) }).map(|p| NewPosixAcl(PosixAcl(p)))
    }

    /// Entry @i set to (@tag, @perm, @id), the id in the initial user
    /// namespace, ignored for a tag that has none.
    pub(crate) fn set(&mut self, i: usize, (tag, perm, id): (u16, u16, u32)) {
        assert!(i < self.0.count(), "entry {i} of an ACL of {}", self.0.count());
        // in bounds, and ours alone
        let e = unsafe { &mut *(&raw mut (*self.0.as_raw()).a_entries).cast::<c::posix_acl_entry>().add(i) };
        e.e_tag  = tag as i16;
        e.e_perm = perm;
        // the kernel's, which take any id
        unsafe {
            match tag as u32 {
                c::ACL_USER  => *e.__bindgen_anon_1.e_uid.as_mut() = c::make_kuid(&raw mut c::init_user_ns, id),
                c::ACL_GROUP => *e.__bindgen_anon_1.e_gid.as_mut() = c::make_kgid(&raw mut c::init_user_ns, id),
                _            => {}
            }
        }
    }
}

impl From<NewPosixAcl> for PosixAcl {
    fn from(acl: NewPosixAcl) -> Self {
        acl.0
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
    AclType = i32 => |r| AclType::from_vfs(r);
    /// The VFS's reference, None for no ACL.
    Option<ManuallyDrop<PosixAcl>> = *mut c::posix_acl => |r| PosixAcl::borrow_raw(r);
}

into_raw! {
    /// NULL for no ACL; the ACL's reference ours to give.
    Result<Option<PosixAcl>, BchError> => *mut c::posix_acl:
        |r| r.map_or_else(err_class_ptr, |acl| acl.map_or(core::ptr::null_mut(), PosixAcl::into_raw));
}
