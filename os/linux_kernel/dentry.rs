// SPDX-License-Identifier: GPL-2.0

//! The VFS's dentry, for Rust's VFS entry points. A Dentry is one borrowed
//! for a VFS call, which holds a reference on it for as long as it runs:
//! that's 'a.
//!
//! d_parent changes under rename, and the old parent can go once it has; it's
//! read under d_lock, which rename takes to change it.
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

use core::marker::PhantomData;
use core::ptr::NonNull;

use kernel::sync::aref::ARef;

use crate::c;
use crate::errcode::{errptr_to_result, BchError};
use crate::locking::{Locked, SpinLockIrq};
use crate::os::file::Any;
use crate::os::ops::{err_class_ptr, from_raw};
use crate::os::super_block::IsInode;
use crate::os::vfs_inode::VfsInode;
use crate::util::os_str::{qstr_name, OsStr};

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

from_raw! {
    /// An op's: on the filesystem whose inodes are I.
    Dentry<'a, I> [I: IsInode]         = *mut c::dentry => |r| Dentry::borrow_raw(r);
    /// get_link's, None in an RCU walk.
    Option<Dentry<'a, I>> [I: IsInode] = *mut c::dentry => |r| (!r.is_null()).then(|| Dentry::borrow_raw(r));
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

    /// Its inode, as the VFS has it - None for a negative dentry; stable as
    /// inode_opt()'s.
    fn vfs_inode_opt(&self) -> Option<&'a VfsInode> {
        unsafe { (*self.raw).d_inode.cast::<VfsInode>().as_ref() }
    }

    /// DCACHE_OP_HASH and DCACHE_OP_COMPARE, set while @inode is casefolded.
    /// d_lock held.
    #[cfg(CONFIG_UNICODE)]
    fn set_casefold_flags(&self, inode: &VfsInode) {
        let op = c::DCACHE_OP_HASH | c::DCACHE_OP_COMPARE;
        // SAFETY: referenced; d_flags is written under d_lock, which is held,
        // and read without it - READ_ONCE(), WRITE_ONCE()
        unsafe {
            let p = &raw mut (*self.raw).d_flags;
            let flags = core::ptr::read_volatile(p);
            core::ptr::write_volatile(p, if inode.flag(c::S_CASEFOLD) { flags | op } else { flags & !op });
        }
    }

    /// Casefolding was turned on or off on this directory: drop its cached
    /// children, whose names were folded the old way, and use its ops or
    /// not.
    pub fn casefold_changed(&self) {
        #[cfg(CONFIG_UNICODE)]
        if let Some(dir) = self.vfs_inode_opt().filter(|i| i.is_dir()) {
            self.shrink_children();
            let _d_lock = self.d_lock().lock();
            self.set_casefold_flags(dir);
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
            unsafe { (*self.raw).d_op = &*CASEFOLD_OPS };
            self.set_casefold_flags(inode);
        }
        #[cfg(not(CONFIG_UNICODE))]
        let _ = inode;
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

    /// Give it @inode - None for a negative dentry - or give @inode's alias
    /// instead, for ->lookup to return: d_splice_alias(), which takes the
    /// reference, whatever it returns.
    pub fn splice_alias(self, inode: Option<ARef<I>>) -> *mut c::dentry {
        let inode = inode.map_or(core::ptr::null_mut(), |i| ARef::into_raw(i).as_ptr().cast());
        unsafe { c::d_splice_alias(inode, self.raw) }
    }

    /// Make this negative dentry @inode's: d_instantiate(), which takes the
    /// reference.
    pub fn instantiate(&self, inode: ARef<I>) {
        unsafe { c::d_instantiate(self.raw, ARef::into_raw(inode).as_ptr().cast()) }
    }

    /// A dentry for @inode, connected or not, as exportfs wants one - its
    /// casefold ops set, for a directory: d_obtain_alias(), which takes the
    /// reference. An error, classed, as an ERR_PTR.
    pub fn obtain_alias(inode: Result<ARef<I>, BchError>) -> *mut c::dentry {
        let inode = match inode {
            Ok(inode) => ARef::into_raw(inode).cast::<c::inode>(),
            Err(e)    => return err_class_ptr(e),
        };
        let dentry = unsafe { c::d_obtain_alias(inode.as_ptr()) };
        if errptr_to_result(NonNull::new(dentry).expect("a dentry, or an error")).is_ok() {
            // SAFETY: the dentry's, which holds the inode's reference
            unsafe { Dentry::<I>::borrow_raw(dentry).set_casefold_ops(&*inode.as_ptr().cast::<VfsInode>()) };
        }
        dentry
    }
}
