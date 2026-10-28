// SPDX-License-Identifier: GPL-2.0

//! The VFS's inode, InodeInfo - C's struct bch_inode_info - for Rust's VFS
//! entry points. One borrowed for a VFS call is a &InodeInfo: the call holds
//! a reference on the inode for as long as it runs. InodeRef is one Rust holds
//! a reference on itself.
//!
//! ei_inode, the inode as last written, is read without a lock: its writers
//! are serialized by the inode's btree position, which a transaction still
//! holds locked after its commit, through update_after_write().
//!
//! Its struct inode is @v, a VfsInode (vfs_inode.rs): what Rust does of the
//! VFS inode, it does through that.
//!
//! ei_update_lock, UpdateLock, serializes what the btree doesn't: a project
//! quota transfer and the inode write that goes with it - set_projid(),
//! rename's and the reinherit ioctl's - so the inode and its quota agree on
//! the project.

use core::ffi::{c_int, c_void};
use core::mem::offset_of;
use core::ptr::NonNull;
use core::sync::atomic::Ordering::Relaxed;

use crate::btree::iter::{BtreeTrans, TransAttempt};
use crate::c;
use crate::errcode::{errptr_to_result, ret_to_c, ret_to_result_void as ret_to_result, BchError};
use crate::fs::Fs;
use crate::locking::{Guard, Lock};
use cstruct_macros::c_extern;
use kernel::sync::aref::{ARef, AlwaysRefCounted};

use super::vfs_inode::VfsInode;

pub use c::InodeInfo;

c_extern! {
    pub fn bch2_vfs_inode_get_trans(trans: *mut c::btree_trans, inum: c::subvol_inum,
                                    func: *const crate::util::ffi::c_char) -> *mut c::bch_inode_info;
}

pub type InodeRef = ARef<InodeInfo>;

// SAFETY: its count is its struct inode's, which is first
unsafe impl AlwaysRefCounted for InodeInfo {
    fn inc_ref(&self) {
        self.vfs().inc_ref()
    }

    unsafe fn dec_ref(obj: NonNull<Self>) {
        unsafe { VfsInode::dec_ref(obj.cast()) }
    }
}

/// <linux/fs.h>'s ATTR_*: an iattr's ia_valid, and update_after_write()'s
/// @fields.
pub const ATTR_MODE:  u32 = 1 << 0;
pub const ATTR_UID:   u32 = 1 << 1;
pub const ATTR_GID:   u32 = 1 << 2;
pub const ATTR_SIZE:  u32 = 1 << 3;
pub const ATTR_ATIME: u32 = 1 << 4;
pub const ATTR_MTIME: u32 = 1 << 5;
pub const ATTR_CTIME: u32 = 1 << 6;

// to_bch_ei(): an InodeInfo is where its struct inode is
const _: () = assert!(offset_of!(InodeInfo, v) == 0);

impl InodeInfo {
    /// # Safety
    /// @vinode is one of ours, live for 'a: the VFS holds a reference on it,
    /// or otherwise keeps it from eviction.
    pub unsafe fn from_inode<'a>(vinode: *mut c::inode) -> &'a InodeInfo {
        // SAFETY: the caller's; @v is first
        unsafe { &*vinode.cast::<InodeInfo>() }
    }

    /// An inode C returns referenced, or an ERR_PTR(): the reference ours
    /// now.
    ///
    /// # Safety
    /// @ei is one of ours with a reference the caller hands over, or an
    /// ERR_PTR().
    pub(crate) unsafe fn from_c(ei: *mut c::bch_inode_info) -> Result<InodeRef, BchError> {
        let ei = errptr_to_result(NonNull::new(ei).expect("an inode, or an error"))?;
        Ok(unsafe { ARef::from_raw(ei) })
    }

    /// @inum's, looked up in @t, the inode cache first.
    pub fn get_trans(t: &TransAttempt<'_, '_>, inum: c::subvol_inum) -> Result<InodeRef, BchError> {
        unsafe { Self::from_c(bch2_vfs_inode_get_trans(t.raw(), inum, crate::c_function_name!().as_ptr().cast())) }
    }

    pub fn raw(&self) -> *mut c::bch_inode_info {
        core::ptr::from_ref(self).cast_mut()
    }

    pub fn fs(&self) -> &Fs {
        self.vfs().sb().fs()
    }

    pub fn vfs(&self) -> &VfsInode {
        &self.v
    }

    pub fn inum(&self) -> c::subvol_inum {
        self.ei_inum_hash.inum
    }

    /// A copy of ei_inode.
    pub fn inode(&self) -> c::bch_inode_unpacked {
        self.ei_inode.get()
    }

    pub fn flag(&self, f: c::bch_inode_info_flags) -> bool {
        self.ei_flags.load(Relaxed) & (1 << f as u32) != 0
    }

    /// Update the VFS inode to @inode, just written and committed in attempt
    /// @t, which still holds the inode's btree position locked. @fields: the
    /// ATTR_* that changed.
    pub fn update_after_write(&self, t: &TransAttempt<'_, '_>, inode: &mut c::bch_inode_unpacked,
                              fields: u32) {
        unsafe { c::bch2_inode_update_after_write(t.raw(), self.raw(), inode, fields) }
    }

    pub fn update_lock(&self) -> UpdateLock<'_> {
        UpdateLock { inode: self, _guard: Lock::from_c(&self.ei_update_lock).lock() }
    }
}

/// ei_update_lock, held.
pub struct UpdateLock<'i> {
    inode:  &'i InodeInfo,
    _guard: Guard<'i, c::mutex>,
}

impl UpdateLock<'_> {
    /// Move the inode to project @projid, quota and all.
    pub fn set_projid(&self, projid: u32) -> Result<(), BchError> {
        let inode = self.inode;
        ret_to_result(unsafe { c::rust_ei_set_projid(inode.fs().as_ptr(), inode.raw(), projid) })
    }

    /// Change the inode with @set, in a transaction of its own that's
    /// committed, then update the VFS inode - @fields, the ATTR_* that
    /// changed. @set runs in each attempt, on the inode as it is in the btree.
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
            // SAFETY: bch2_write_inode() calls @set in its attempt
            ret_to_c(set(&unsafe { trans.attempt_in_progress() }, unsafe { &mut *bi }))
        }

        let inode = self.inode;
        ret_to_result(unsafe {
            c::bch2_write_inode(inode.fs().as_ptr(), inode.raw(), Some(set_fn::<F>),
                                &mut set as *mut F as *mut c_void, fields)
        })
    }
}
