// SPDX-License-Identifier: GPL-2.0

//! A dentry's bcachefs side: its inode, and casefolding; the dentry is
//! os::dentry's.
//!
//! A directory's dentry has the kernel's casefolding d_hash and d_compare as
//! its ops from when its inode is attached - d_alloc() runs before that, so
//! it can't choose them - but they're used only while DCACHE_OP_HASH and
//! DCACHE_OP_COMPARE are set, while the directory is casefolded: the RCU
//! lookup checks the parent's flag, so other directories keep the inline
//! compare. A change of casefolding changes only the flags - never the ops,
//! so a lockless reader never sees a flag without its op. The flags change
//! under d_lock, as a single write: d_wait_lookup() sets
//! DCACHE_LOOKUP_WAITERS under it, which an unlocked read-modify-write could
//! erase.

use core::ffi::CStr;
use core::ptr::NonNull;

use crate::c;
use crate::errcode::{errptr_to_result, BchError};
use crate::inode_opts::InodeOpt;
use crate::os::dentry::Dentry;
use crate::xattr::XattrType;

use kernel::sync::aref::ARef;

use super::inode::{InodeInfo, InodeRef};
use super::vfs_inode::VfsInode;

#[cfg(CONFIG_UNICODE)]
// SAFETY, of the CStatic: immutable, its only parts that aren't Sync pointers
static CASEFOLD_OPS: crate::types::CStatic<c::dentry_operations> = unsafe {
    crate::types::CStatic::new(c::dentry_operations {
        d_hash:    Some(c::generic_ci_d_hash),
        d_compare: Some(c::generic_ci_d_compare),
        // zero is NULL, an op not implemented
        ..::pin_init::zeroed()
    })
};

impl<'a> Dentry<'a> {
    /// A positive dentry's inode.
    pub fn inode(&self) -> &'a InodeInfo {
        self.inode_opt().expect("the inode of a negative dentry")
    }

    pub fn inode_opt(&self) -> Option<&'a InodeInfo> {
        // SAFETY: ours, as borrow_raw() was promised. A referenced positive
        // dentry stays positive - d_delete() only unhashes one with other
        // references - and a negative one stays negative while the call holds
        // its directory locked, as a namespace operation does
        let inode = unsafe { (*self.as_ptr()).d_inode };
        (!inode.is_null()).then(|| unsafe { InodeInfo::from_inode(inode) })
    }

    /// Inode option @opt of the parent directory - None at the root, which
    /// has none: what clearing it on this inode inherits.
    pub fn parent_inode_opt(&self, opt: InodeOpt) -> Option<u64> {
        let _d_lock = self.d_lock().lock();

        // A root is its own parent:
        let parent = unsafe { (*self.as_ptr()).d_parent };
        if parent == self.as_ptr() {
            return None;
        }
        // SAFETY: held by this dentry while d_lock is; a directory of ours
        let dir = unsafe { InodeInfo::from_inode((*parent).d_inode) };
        Some(opt.get(&dir.inode()))
    }

    /// DCACHE_OP_HASH and DCACHE_OP_COMPARE, set while @inode is casefolded.
    /// d_lock held.
    #[cfg(CONFIG_UNICODE)]
    fn set_casefold_flags(&self, inode: &VfsInode) {
        let op = c::DCACHE_OP_HASH | c::DCACHE_OP_COMPARE;
        // SAFETY: referenced; d_flags is written under d_lock, which is held,
        // and read without it - READ_ONCE(), WRITE_ONCE()
        unsafe {
            let p = &raw mut (*self.as_ptr()).d_flags;
            let flags = core::ptr::read_volatile(p);
            core::ptr::write_volatile(p, if inode.flag(c::S_CASEFOLD) { flags | op } else { flags & !op });
        }
    }

    /// Casefolding was turned on or off on this directory: drop its cached
    /// children, whose names were folded the old way, and use its ops or
    /// not.
    pub fn casefold_changed(&self) {
        #[cfg(CONFIG_UNICODE)]
        if let Some(dir) = self.inode_opt().filter(|i| i.vfs().is_dir()) {
            unsafe { c::shrink_dcache_parent(self.as_ptr()) };
            let _d_lock = self.d_lock().lock();
            self.set_casefold_flags(dir.vfs());
        }
    }

    /// For a directory @inode, before it's this dentry's: the dentry ops that
    /// fold names, which it uses while @inode is casefolded.
    pub fn set_casefold_ops(&self, inode: &VfsInode) {
        #[cfg(CONFIG_UNICODE)]
        if inode.is_dir() {
            let _d_lock = self.d_lock().lock();
            // SAFETY: referenced; d_op is set under d_lock, before the flags
            // that have it used
            unsafe { (*self.as_ptr()).d_op = &*CASEFOLD_OPS };
            self.set_casefold_flags(inode);
        }
        #[cfg(not(CONFIG_UNICODE))]
        let _ = inode;
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
