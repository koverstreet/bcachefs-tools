// SPDX-License-Identifier: GPL-2.0

//! The VFS's side of vfs_ops! (util/ops.rs): what its ops are passed and
//! return, as FromRaw and IntoRaw impls, and SelfOf's &self - what the
//! superblock's s_fs_info points to, found from whatever the op is passed.

use core::ffi::{c_int, c_void, CStr};
use core::mem::ManuallyDrop;
use core::ptr::NonNull;

use crate::acl::{AclType, PosixAcl};
use crate::c;
use crate::errcode::BchError;
use crate::util::ffi::c_char;
use crate::util::ops::{from_raw, into_raw, SelfOf};
use crate::util::seq_file::SeqFile;

use super::dentry::Dentry;
use super::export::NameBuf;
use super::folio::{Folio, FolioGuard, LockedFolio, Mapping, Readahead};
use super::idmap::{MntIdmap, RawIdmap};
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
    MntIdmap<'a>          = RawIdmap            => |r| MntIdmap::borrow_raw(r);
    /// One this thread is writing.
    &'a SeqFile           = *mut c::seq_file    => |r| SeqFile::from_raw(r);
    &'a CStr              = *const c_char       => |r| CStr::from_ptr(r.cast());
    AclType               = c_int               => |r| AclType::from_vfs(r);
    /// The VFS's reference, None for no ACL.
    Option<ManuallyDrop<PosixAcl>> = *mut c::posix_acl => |r| PosixAcl::borrow_raw(r);
    &'a mut c::kstat      = *mut c::kstat       => |r| &mut *r;
    &'a mut c::iattr      = *mut c::iattr       => |r| &mut *r;
    &'a mut c::file_kattr = *mut c::file_kattr  => |r| &mut *r;
    &'a mut c::delayed_call = *mut c::delayed_call => |r| &mut *r;
    &'a mut c::fiemap_extent_info = *mut c::fiemap_extent_info => |r| &mut *r;
    /// The kernel crate's is fs.h's declaration, opaque; codegen's the same.
    &'a mut c::kstatfs    = *mut kernel::bindings::kstatfs => |r| &mut *r.cast();
    /// get_name's, NAME_MAX + 1 bytes.
    &'a mut NameBuf       = *mut c_char         => |r| &mut *r.cast();
}

#[cfg(linux_ge = "6.17")]
from_raw! {
    /// mmap_prepare's.
    &'a mut c::vm_area_desc = *mut c::vm_area_desc => |r| &mut *r;
}

#[cfg(not(linux_ge = "6.17"))]
from_raw! {
    /// mmap's.
    &'a mut c::vm_area_struct = *mut c::vm_area_struct => |r| &mut *r;
}

from_raw! {
    Mapping               = *mut c::address_space => |r| Mapping::from_raw(r);
    Folio                 = *mut c::folio         => |r| Folio::from_raw(r);
    /// read_folio()'s: locked, its lock ours to give up, not its ref.
    FolioGuard<'a>        = *mut c::folio         => |r| FolioGuard::adopt(Folio::from_raw(r));
    Readahead<'a>         = *mut c::readahead_control => |r| Readahead::from_raw(r);
    /// write_end()'s: write_begin()'s, its lock and ref to give up.
    LockedFolio           = *mut c::folio         => |r| LockedFolio::from_raw(r);
    /// write_begin()'s out parameters.
    &'a mut *mut c::folio = *mut *mut c::folio    => |r| &mut *r;
    &'a mut *mut c_void   = *mut *mut c_void      => |r| &mut *r;
}

/// write_begin() and write_end()'s first argument, which they don't use: the
/// kiocb from 6.17, the file before.
pub(crate) struct VfsWriter;

#[cfg(linux_ge = "6.17")]
from_raw! {
    VfsWriter = *const c::kiocb => |_r| VfsWriter;
}

#[cfg(not(linux_ge = "6.17"))]
from_raw! {
    VfsWriter = *mut c::file => |_r| VfsWriter;
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
