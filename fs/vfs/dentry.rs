// SPDX-License-Identifier: GPL-2.0

//! A dentry's bcachefs side: our dentries are InodeInfo's, and what of them
//! is bcachefs's - inode options, xattr prefixes; the dentry itself is
//! os::dentry's.

use core::ffi::CStr;

use crate::posix::inode_opts::InodeOpt;
use crate::posix::xattr::XattrType;

use super::inode::InodeInfo;

/// Ours: a dentry of this filesystem, its inode an InodeInfo.
pub type Dentry<'a> = crate::os::dentry::Dentry<'a, InodeInfo>;

impl<'a> Dentry<'a> {
    /// Inode option @opt of the parent directory - None at the root, which
    /// has none: what clearing it on this inode inherits.
    pub fn parent_inode_opt(&self, opt: InodeOpt) -> Option<u64> {
        self.with_parent_inode(|dir| dir.map(|dir| opt.get(&dir.inode())))
    }

    /// The prefix xattrs of @ty list under, here - None for a type that
    /// isn't listed, or that the caller may not see.
    pub fn xattr_list_prefix(&self, ty: XattrType) -> Option<&'static CStr> {
        crate::posix::xattr::list_prefix(ty, *self)
    }
}
