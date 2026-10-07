// SPDX-License-Identifier: GPL-2.0

//! The VFS's dentry, for Rust's VFS entry points: opaque, as struct inode
//! is - see inode.rs - and reached through vfs/rust.h's shims. A Dentry is
//! one borrowed for a VFS call, which holds a reference on it for as long as
//! it runs: that's 'a.

use core::marker::PhantomData;

use crate::c;
use crate::opts::InodeOpt;

use super::inode::InodeInfo;

#[derive(Clone, Copy)]
pub struct Dentry<'a> {
    raw:  *mut c::dentry,
    _ref: PhantomData<&'a c::dentry>,
}

impl<'a> Dentry<'a> {
    /// # Safety
    /// @raw is a dentry the VFS holds a reference on, for 'a, with one of our
    /// inodes.
    pub unsafe fn borrow_raw(raw: *mut c::dentry) -> Self {
        Dentry { raw, _ref: PhantomData }
    }

    pub fn inode(&self) -> InodeInfo<'a> {
        // A positive dentry of ours, as borrow_raw() was promised:
        unsafe { InodeInfo::from_ei(c::rust_dentry_ei(self.raw)) }
    }

    /// Inode option @opt of the parent directory - None at the root, which
    /// has none: what clearing it on this inode inherits.
    pub fn parent_inode_opt(&self, opt: InodeOpt) -> Option<u64> {
        let mut v = 0;
        unsafe { c::rust_dentry_parent_inode_opt(self.raw, opt.id() as u32, &mut v) }
            .then_some(v)
    }

    /// Casefolding was turned on or off on this directory: drop its cached
    /// children, whose names were folded the old way - as
    /// bch2_dir_casefold_changed().
    pub fn casefold_changed(&self) {
        unsafe { c::rust_dir_casefold_changed(self.raw) }
    }
}
