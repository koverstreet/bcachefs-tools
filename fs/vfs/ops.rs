// SPDX-License-Identifier: GPL-2.0

//! The VFS's side of vfs_ops! (os/linux_kernel/ops.rs): what its ops are
//! passed that's bcachefs's, as FromRaw impls.

use core::ptr::NonNull;

use crate::c;
use crate::os::ops::from_raw;
use crate::util::ffi::c_char;

use super::export::NameBuf;
use super::idmap::{MntIdmap, RawIdmap};
use super::inode::InodeInfo;

from_raw! {
    &'a InodeInfo         = *mut c::inode       => |r| InodeInfo::from_inode(r);
    /// A parent, which may not be given.
    Option<&'a InodeInfo> = *mut c::inode       => |r| (!r.is_null()).then(|| InodeInfo::from_inode(r));
    /// One the VFS is done with, to free: not borrowed, nor its superblock.
    NonNull<InodeInfo>    = *mut c::inode       => |r| NonNull::new(r).expect("an inode").cast();
    MntIdmap<'a>          = RawIdmap            => |r| MntIdmap::borrow_raw(r);
    /// get_name's, NAME_MAX + 1 bytes.
    &'a mut NameBuf       = *mut c_char         => |r| &mut *r.cast();
}
