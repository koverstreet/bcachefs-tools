// SPDX-License-Identifier: GPL-2.0

//! The VFS's dentry, for Rust's VFS entry points. A Dentry is one borrowed
//! for a VFS call, which holds a reference on it for as long as it runs:
//! that's 'a.
//!
//! d_parent changes under rename, and the old parent can go once it has; it's
//! read under d_lock, which rename takes to change it.

use core::marker::PhantomData;

use crate::c;
use crate::locking::{Locked, SpinLockIrq};
use crate::os::file::Any;
use crate::os::ops::FromRaw;
use crate::os::super_block::IsInode;
use crate::util::os_str::{qstr_name, OsStr};

/// A dentry whose inode, if it has one, is an I.
pub struct Dentry<'a, I = Any> {
    raw:  *mut c::dentry,
    _ref: PhantomData<(&'a c::dentry, &'a I)>,
}

impl<I> Clone for Dentry<'_, I> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<I> Copy for Dentry<'_, I> {}

/// An op's: on the filesystem whose inodes are I.
impl<'a, I: IsInode> FromRaw for Dentry<'a, I> {
    type Raw = *mut c::dentry;
    unsafe fn from_raw(r: *mut c::dentry) -> Self { unsafe { Dentry::borrow_raw(r) } }
}

/// get_link's, None in an RCU walk.
impl<'a, I: IsInode> FromRaw for Option<Dentry<'a, I>> {
    type Raw = *mut c::dentry;
    unsafe fn from_raw(r: *mut c::dentry) -> Self { (!r.is_null()).then(|| unsafe { Dentry::borrow_raw(r) }) }
}

impl<'a, I> Dentry<'a, I> {
    /// # Safety
    /// @raw is a dentry the VFS holds a reference on, for 'a - its inode, if
    /// it has one, an I.
    pub unsafe fn borrow_raw(raw: *mut c::dentry) -> Self {
        Dentry { raw, _ref: PhantomData }
    }

    pub fn as_ptr(&self) -> *mut c::dentry {
        self.raw
    }

    /// Stable while its directory is locked, as the namespace operations
    /// hold it.
    pub fn name(&self) -> &'a OsStr {
        unsafe { qstr_name(self.qstr()) }
    }

    /// name(), as the VFS has it.
    pub fn qstr(&self) -> &'a c::qstr {
        unsafe {
            // 6.18 made d_name a const union member, __d_name its writable alias
            #[cfg(linux_ge = "6.18")]
            let name = &(*self.raw).__bindgen_anon_1.d_name;
            #[cfg(not(linux_ge = "6.18"))]
            let name = &(*self.raw).d_name;
            name
        }
    }

    /// Drop it, and what's cached under it.
    pub fn invalidate(&self) {
        unsafe { c::d_invalidate(self.raw) }
    }

    /// What's cached under it and unused dropped: shrink_dcache_parent().
    pub fn shrink_children(&self) {
        unsafe { c::shrink_dcache_parent(self.raw) }
    }

    pub(crate) fn d_lock(&self) -> &Locked<SpinLockIrq> {
        // SAFETY: referenced, so live; d_lock is a #define, d_lockref.lock, and
        // SpinLockIrq is the kernel's spinlock_t, in an Opaque
        Locked::from_c(unsafe {
            &*(&raw const (*self.raw).d_lockref.__bindgen_anon_1.__bindgen_anon_1.lock).cast::<SpinLockIrq>()
        })
    }
}

impl<'a, I: IsInode> Dentry<'a, I> {
    /// A positive dentry's inode.
    pub fn inode(&self) -> &'a I {
        self.inode_opt().expect("the inode of a negative dentry")
    }

    pub fn inode_opt(&self) -> Option<&'a I> {
        // SAFETY: an I, as borrow_raw() was promised. A referenced positive
        // dentry stays positive - d_delete() only unhashes one with other
        // references - and a negative one stays negative while the call holds
        // its directory locked, as a namespace operation does
        unsafe { (*self.raw).d_inode.cast::<I>().as_ref() }
    }

    /// @f, of the parent directory's inode - None at a root, its own parent:
    /// under d_lock, which holds the parent while it's read.
    pub fn with_parent_inode<R>(&self, f: impl FnOnce(Option<&I>) -> R) -> R {
        let _d_lock = self.d_lock().lock();

        let parent = unsafe { (*self.raw).d_parent };
        if parent == self.raw {
            return f(None);
        }
        // SAFETY: held by this dentry while d_lock is; a directory of the
        // same filesystem
        f(Some(unsafe { &*(*parent).d_inode.cast::<I>() }))
    }
}
