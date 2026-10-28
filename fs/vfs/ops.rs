// SPDX-License-Identifier: GPL-2.0

//! The VFS's side of vfs_ops! (util/ops.rs): what its ops are passed and
//! return, as FromRaw and IntoRaw impls, and SelfOf's &self - what the
//! superblock's s_fs_info points to, found from whatever the op is passed.

use core::ffi::CStr;
use core::mem::ManuallyDrop;
use core::ptr::NonNull;

use crate::acl::PosixAcl;
use crate::c;
use crate::errcode::BchError;
use crate::util::ffi::c_char;
use crate::util::ops::{from_raw, into_raw, SelfOf};
use crate::util::seq_file::SeqFile;

use super::dentry::Dentry;
use super::idmap::MntIdmap;
use super::inode::InodeInfo;
use super::path::Path;
use super::super_block::SuperBlock;

// s_fs_info is set at mount, before the superblock has an inode, to the T
// that implements its traits: SelfOf's contract

impl<T: 'static> SelfOf<T> for *mut c::super_block {
    type Out = T;
    unsafe fn self_of<'a>(&self) -> &'a T { unsafe { &*(**self).s_fs_info.cast::<T>() } }
}

impl<T: 'static> SelfOf<T> for *mut c::inode {
    type Out = T;
    unsafe fn self_of<'a>(&self) -> &'a T { unsafe { SelfOf::<T>::self_of(&(**self).i_sb) } }
}

impl<T: 'static> SelfOf<T> for *mut c::dentry {
    type Out = T;
    unsafe fn self_of<'a>(&self) -> &'a T { unsafe { SelfOf::<T>::self_of(&(**self).d_sb) } }
}

// ── Arguments ────────────────────────────────────────────────────────────

from_raw! {
    &'a SuperBlock        = *mut c::super_block => |r| SuperBlock::from_raw(r);
    &'a InodeInfo         = *mut c::inode       => |r| InodeInfo::from_inode(r);
    /// A parent, which may not be given.
    Option<&'a InodeInfo> = *mut c::inode       => |r| (!r.is_null()).then(|| InodeInfo::from_inode(r));
    /// One the VFS is done with, to free: not borrowed, nor its superblock.
    NonNull<InodeInfo>    = *mut c::inode       => |r| NonNull::new(r).expect("an inode").cast();
    Dentry<'a>            = *mut c::dentry      => |r| Dentry::borrow_raw(r);
    /// get_link's, None in an RCU walk.
    Option<Dentry<'a>>    = *mut c::dentry      => |r| (!r.is_null()).then(|| Dentry::borrow_raw(r));
    &'a Path              = *const c::path      => |r| &*r.cast();
    MntIdmap<'a>          = *mut c::mnt_idmap   => |r| MntIdmap::borrow_raw(r);
    /// One this thread is writing.
    &'a SeqFile           = *mut c::seq_file    => |r| SeqFile::from_raw(r);
    &'a CStr              = *const c_char       => |r| CStr::from_ptr(r.cast());
    /// The VFS's reference, None for no ACL.
    Option<ManuallyDrop<PosixAcl>> = *mut c::posix_acl => |r| PosixAcl::borrow_raw(r);
    &'a mut c::kstat      = *mut c::kstat       => |r| &mut *r;
    &'a mut c::iattr      = *mut c::iattr       => |r| &mut *r;
    &'a mut c::file_kattr = *mut c::file_kattr  => |r| &mut *r;
    &'a mut c::delayed_call = *mut c::delayed_call => |r| &mut *r;
    &'a mut c::fiemap_extent_info = *mut c::fiemap_extent_info => |r| &mut *r;
    /// The kernel crate's is fs.h's declaration, opaque; codegen's the same.
    &'a mut c::kstatfs    = *mut kernel::bindings::kstatfs => |r| &mut *r.cast();
}

// ── Returns ──────────────────────────────────────────────────────────────

/// @e, classed, as an ERR_PTR().
pub(crate) fn err_class_ptr<T>(e: BchError) -> *mut T {
    e.class() as isize as *mut T
}

into_raw! {
    /// mkdir's: NULL for the dentry it was given, instantiated.
    Result<(), BchError>    => *mut c::dentry: |r| r.map_or_else(err_class_ptr, |()| core::ptr::null_mut());
    Result<usize, BchError> => isize: |r| r.map_or_else(|e| e.class() as isize, |n| n as isize);
    /// NULL for no ACL; the ACL's reference ours to give.
    Result<Option<PosixAcl>, BchError> => *mut c::posix_acl:
        |r| r.map_or_else(err_class_ptr, |acl| acl.map_or(core::ptr::null_mut(), PosixAcl::into_raw));
    /// alloc_inode's: NULL if there isn't one.
    Result<NonNull<c::inode>, BchError> => *mut c::inode: |r| r.map_or(core::ptr::null_mut(), NonNull::as_ptr);
}
