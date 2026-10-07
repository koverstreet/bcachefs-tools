// SPDX-License-Identifier: GPL-2.0

//! The VFS's inode, struct bch_inode_info, for Rust's VFS entry points -
//! xattr's and acl's.
//!
//! It's kernel types all the way down - struct inode, its locks, the
//! pagecache - so Rust doesn't bind its layout: it's opaque, and reached
//! through vfs/rust.h's shims. An InodeInfo is one borrowed for a VFS call,
//! which holds a reference on the inode for as long as it runs: that's 'a.
//!
//! ei_update_lock serializes updates to the inode from the VFS side - read
//! the inode, change it, write it back. Nothing asserts it's held; what needs
//! it hangs off the guard, UpdateLock, so it can't be called without.
//! ei_inode itself is protected by the btree node lock, not this one - see
//! bch2_write_inode_trans().

use core::ffi::{c_int, c_void};
use core::marker::PhantomData;
use core::mem::ManuallyDrop;

use crate::btree::iter::{BtreeTrans, TransAttempt};
use crate::c;
use crate::errcode::{ret_to_c, ret_to_result_void as ret_to_result, BchError};
use crate::fs::Fs;

/// <linux/fs.h>'s ATTR_*: what update_after_write() updates in the VFS
/// inode, besides what it always does.
pub const ATTR_MODE:  u32 = 1 << 0;
pub const ATTR_CTIME: u32 = 1 << 6;

pub struct InodeInfo<'a> {
    ei:   *mut c::bch_inode_info,
    fs:   ManuallyDrop<Fs>,
    _ref: PhantomData<&'a c::bch_inode_info>,
}

impl InodeInfo<'_> {
    /// # Safety
    /// @ei is an inode the VFS holds a reference on, for 'a.
    pub unsafe fn from_ei(ei: *mut c::bch_inode_info) -> Self {
        InodeInfo { ei, fs: unsafe { Fs::borrow_raw(c::rust_ei_fs(ei)) }, _ref: PhantomData }
    }

    /// # Safety
    /// As from_ei(): @vinode is one of ours.
    pub unsafe fn from_inode(vinode: *mut c::inode) -> Self {
        unsafe { Self::from_ei(c::rust_to_bch_ei(vinode)) }
    }

    pub fn raw(&self) -> *mut c::bch_inode_info {
        self.ei
    }

    /// The struct inode embedded in it, for the VFS's own functions.
    pub fn vinode(&self) -> *mut c::inode {
        unsafe { c::rust_ei_vinode(self.ei) }
    }

    pub fn fs(&self) -> &Fs {
        &self.fs
    }

    pub fn inum(&self) -> c::subvol_inum {
        unsafe { c::rust_ei_inum(self.ei) }
    }

    /// The inode as last written, as the VFS inode caches it: ei_inode.
    pub fn inode(&self) -> &c::bch_inode_unpacked {
        unsafe { &*c::rust_ei_inode(self.ei) }
    }

    /// Update the VFS inode to @inode, just written in @trans, which still
    /// holds the inode's btree position locked - as
    /// bch2_inode_update_after_write(). @fields: the ATTR_* that changed.
    pub fn update_after_write(&self, trans: &BtreeTrans<'_>, inode: &mut c::bch_inode_unpacked,
                              fields: u32) {
        unsafe { c::bch2_inode_update_after_write(trans.raw(), self.ei, inode, fields) }
    }

    /// Take ei_update_lock, until the guard is dropped.
    pub fn update_lock(&self) -> UpdateLock<'_> {
        unsafe { c::rust_ei_update_lock(self.ei) };
        UpdateLock { inode: self }
    }
}

/// ei_update_lock, held.
pub struct UpdateLock<'i> {
    inode: &'i InodeInfo<'i>,
}

impl UpdateLock<'_> {
    /// Move the inode to project @projid, transferring its quota: as
    /// bch2_set_projid().
    pub fn set_projid(&self, projid: u32) -> Result<(), BchError> {
        let inode = self.inode;
        ret_to_result(unsafe { c::rust_ei_set_projid(inode.fs.raw, inode.ei, projid) })
    }

    /// Change the inode with @set, in a transaction of its own that's
    /// committed, and update the VFS inode after - @fields, the ATTR_* that
    /// changed: as bch2_write_inode(). @set runs in each attempt, on the
    /// inode as it is in the btree.
    pub fn write_inode<F>(&self, fields: u32, mut set: F) -> Result<(), BchError>
    where
        F: FnMut(&TransAttempt<'_, '_>, &mut c::bch_inode_unpacked) -> Result<(), BchError>,
    {
        unsafe extern "C" fn set_fn<F>(
            trans: *mut c::btree_trans,
            _ei:   *mut c::bch_inode_info,
            bi:    *mut c::bch_inode_unpacked,
            p:     *mut c_void,
        ) -> c_int
        where
            F: FnMut(&TransAttempt<'_, '_>, &mut c::bch_inode_unpacked) -> Result<(), BchError>,
        {
            // @p is write_inode()'s @set, for the call:
            let set = unsafe { &mut *(p as *mut F) };
            let trans = unsafe { BtreeTrans::borrow_raw(trans) };
            ret_to_c(set(&trans.attempt_in_progress(), unsafe { &mut *bi }))
        }

        let inode = self.inode;
        ret_to_result(unsafe {
            c::bch2_write_inode(inode.fs.raw, inode.ei, Some(set_fn::<F>),
                                &mut set as *mut F as *mut c_void, fields)
        })
    }
}

impl Drop for UpdateLock<'_> {
    fn drop(&mut self) {
        unsafe { c::rust_ei_update_unlock(self.inode.ei) }
    }
}
