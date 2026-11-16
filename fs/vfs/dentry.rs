// SPDX-License-Identifier: GPL-2.0

//! A dentry's bcachefs side: our dentries are InodeInfo's, and what it does
//! with them; the dentry, and casefolding, are os::dentry's.

use core::ffi::CStr;
use core::ptr::NonNull;

use crate::c;
use crate::errcode::{errptr_to_result, BchError};
use crate::inode_opts::InodeOpt;
use crate::xattr::XattrType;

use kernel::sync::aref::ARef;

use super::inode::{InodeInfo, InodeRef};
use crate::os::vfs_inode::VfsInode;

/// Ours: a dentry of this filesystem, its inode an InodeInfo.
pub type Dentry<'a> = crate::os::dentry::Dentry<'a, InodeInfo>;

impl<'a> Dentry<'a> {
    /// Inode option @opt of the parent directory - None at the root, which
    /// has none: what clearing it on this inode inherits.
    pub fn parent_inode_opt(&self, opt: InodeOpt) -> Option<u64> {
        self.with_parent_inode(|dir| dir.map(|dir| opt.get(&dir.inode())))
    }

    /// Give it @inode - None for a negative dentry - or give @inode's alias
    /// instead, for ->lookup to return: d_splice_alias(), which takes the
    /// reference, whatever it returns.
    pub fn splice_alias(self, inode: Option<InodeRef>) -> *mut c::dentry {
        let inode = inode.map_or(core::ptr::null_mut(), |i| ARef::into_raw(i).as_ptr().cast());
        unsafe { c::d_splice_alias(inode, self.as_ptr()) }
    }

    /// Make this negative dentry @inode's: d_instantiate(), which takes the
    /// reference.
    pub fn instantiate(&self, inode: InodeRef) {
        unsafe { c::d_instantiate(self.as_ptr(), ARef::into_raw(inode).as_ptr().cast()) }
    }

    /// A dentry for @inode, connected or not, as exportfs wants one - its
    /// casefold ops set, for a directory: d_obtain_alias(), which takes the
    /// reference. An error, classed, as an ERR_PTR.
    pub fn obtain_alias(inode: Result<InodeRef, BchError>) -> *mut c::dentry {
        let inode = match inode {
            Ok(inode) => inode,
            Err(e)    => return e.class() as isize as *mut c::dentry,
        };
        let vinode = inode.vfs().as_ptr();
        let dentry = unsafe { c::d_obtain_alias(ARef::into_raw(inode).as_ptr().cast()) };
        if errptr_to_result(NonNull::new(dentry).expect("a dentry, or an error")).is_ok() {
            // SAFETY: the dentry's, which holds the inode's reference
            unsafe { Dentry::borrow_raw(dentry).set_casefold_ops(&*vinode.cast::<VfsInode>()) };
        }
        dentry
    }

    /// The prefix xattrs of @ty list under, here - None for a type that
    /// isn't listed, or that the caller may not see.
    pub fn xattr_list_prefix(&self, ty: XattrType) -> Option<&'static CStr> {
        crate::xattr::list_prefix(ty, *self)
    }
}
