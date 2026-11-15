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
use crate::util::os_str::{qstr_name, OsStr};

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

    pub(crate) fn d_lock(&self) -> &Locked<SpinLockIrq> {
        // SAFETY: referenced, so live; d_lock is a #define, d_lockref.lock, and
        // SpinLockIrq is the kernel's spinlock_t, in an Opaque
        Locked::from_c(unsafe {
            &*(&raw const (*self.raw).d_lockref.__bindgen_anon_1.__bindgen_anon_1.lock).cast::<SpinLockIrq>()
        })
    }
}
