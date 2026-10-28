// SPDX-License-Identifier: GPL-2.0

//! The VFS's dentry, for Rust's VFS entry points. A Dentry is one borrowed
//! for a VFS call, which holds a reference on it for as long as it runs:
//! that's 'a.
//!
//! d_parent changes under rename, and the old parent can go once it has; it's
//! read under d_lock, which rename takes to change it.

use core::ffi::CStr;
use core::marker::PhantomData;
use core::ptr::NonNull;

use crate::c;
use crate::errcode::{errptr_to_result, BchError};
use crate::inode_opts::InodeOpt;
use crate::locking::Lock;
use crate::util::os_str::{qstr_name, OsStr};
#[cfg(CONFIG_UNICODE)]
use cstruct_macros::c_extern;

use kernel::sync::aref::ARef;

use super::inode::{InodeInfo, InodeRef};
use super::vfs_inode::VfsInode;

#[cfg(CONFIG_UNICODE)]
c_extern! {
    pub fn bch2_dir_casefold_changed(dentry: *mut c::dentry);
    fn bch2_dentry_set_casefold_ops(dentry: *mut c::dentry, inode: *mut c::inode);
}

#[derive(Clone, Copy)]
pub struct Dentry<'a> {
    raw:  *mut c::dentry,
    _ref: PhantomData<&'a c::dentry>,
}

impl<'a> Dentry<'a> {
    /// # Safety
    /// @raw is a dentry the VFS holds a reference on, for 'a - its inode, if
    /// it has one, one of ours.
    pub unsafe fn borrow_raw(raw: *mut c::dentry) -> Self {
        Dentry { raw, _ref: PhantomData }
    }

    /// Stable while its directory is locked, as the namespace operations
    /// hold it.
    pub fn name(&self) -> &'a OsStr {
        unsafe {
            // 6.18 made d_name a const union member, __d_name its writable alias
            #[cfg(linux_ge = "6.18")]
            let name = &(*self.raw).__bindgen_anon_1.d_name;
            #[cfg(not(linux_ge = "6.18"))]
            let name = &(*self.raw).d_name;
            qstr_name(name)
        }
    }

    /// Drop it, and what's cached under it.
    pub fn invalidate(&self) {
        unsafe { c::d_invalidate(self.raw) }
    }

    /// A positive dentry's inode.
    pub fn inode(&self) -> &'a InodeInfo {
        self.inode_opt().expect("the inode of a negative dentry")
    }

    pub fn inode_opt(&self) -> Option<&'a InodeInfo> {
        // SAFETY: ours, as borrow_raw() was promised. A referenced positive
        // dentry stays positive - d_delete() only unhashes one with other
        // references - and a negative one stays negative while the call holds
        // its directory locked, as a namespace operation does
        let inode = unsafe { (*self.raw).d_inode };
        (!inode.is_null()).then(|| unsafe { InodeInfo::from_inode(inode) })
    }

    /// Inode option @opt of the parent directory - None at the root, which
    /// has none: what clearing it on this inode inherits.
    pub fn parent_inode_opt(&self, opt: InodeOpt) -> Option<u64> {
        // SAFETY: referenced, so live; d_lock is a #define, d_lockref.lock, and
        // c::spinlock_t the kernel's spinlock_t in an Opaque
        let d_lock = unsafe {
            &*(&raw const (*self.raw).d_lockref.__bindgen_anon_1.__bindgen_anon_1.lock).cast::<c::spinlock_t>()
        };
        let _d_lock = Lock::from_c(d_lock).lock();

        // A root is its own parent:
        let parent = unsafe { (*self.raw).d_parent };
        if parent == self.raw {
            return None;
        }
        // SAFETY: held by this dentry while d_lock is; a directory of ours
        let dir = unsafe { InodeInfo::from_inode((*parent).d_inode) };
        Some(opt.get(&dir.inode()))
    }

    /// Casefolding was turned on or off on this directory: drop its cached
    /// children, whose names were folded the old way.
    pub fn casefold_changed(&self) {
        #[cfg(CONFIG_UNICODE)]
        unsafe { bch2_dir_casefold_changed(self.raw) }
    }

    /// For a directory @inode, before it's this dentry's: the dentry ops that
    /// fold names, which it uses while @inode is casefolded.
    pub fn set_casefold_ops(&self, inode: &VfsInode) {
        #[cfg(CONFIG_UNICODE)]
        unsafe { bch2_dentry_set_casefold_ops(self.raw, inode.as_ptr()) }
        #[cfg(not(CONFIG_UNICODE))]
        let _ = inode;
    }

    /// Give it @inode - None for a negative dentry - or give @inode's alias
    /// instead, for ->lookup to return: d_splice_alias(), which takes the
    /// reference, whatever it returns.
    pub fn splice_alias(self, inode: Option<InodeRef>) -> *mut c::dentry {
        let inode = inode.map_or(core::ptr::null_mut(), |i| ARef::into_raw(i).as_ptr().cast());
        unsafe { c::d_splice_alias(inode, self.raw) }
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
    pub fn xattr_list_prefix(&self, ty: c::bch_xattr_type) -> Option<&'static CStr> {
        crate::xattr::list_prefix(ty, self.raw)
    }
}
