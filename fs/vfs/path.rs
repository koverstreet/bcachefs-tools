// SPDX-License-Identifier: GPL-2.0

//! Paths: Path, one the VFS passes an op; and from userspace, resolved and
//! held until they drop: UserPath, a path looked up; and Creating and
//! Removing, a path's last component to create or remove, with its parent
//! locked and its mount's write access held.
//!
//! The VFS's helpers for the last two changed twice. Before 6.18 they're
//! user_path_create() and done_path_create(), and the removal lookup,
//! user_path_locked_at(), takes the parent's lock without the write access -
//! which nests outside it - so that's dropped, the access taken, the lock
//! retaken, and the victim revalidated: still hashed, still the parent's.
//! 7.2 dropped the removal lookup by dirfd and user pointer: it's
//! user_path_at(), then start_removing_dentry() to lock the parent and
//! revalidate the victim under the lock.

use core::ffi::{c_int, c_uint};
use core::mem::MaybeUninit;
use core::ptr::NonNull;

use crate::c;
use crate::errcode::{errptr_to_result, ret_to_result_void, BchError};
use crate::fs::Fs;
use crate::util::ffi::c_char;

use super::dentry::Dentry;

fn user_ptr(addr: u64) -> *const c_char {
    addr as usize as *const c_char
}

/// Whether @dentry is on @fs.
fn on(dentry: *mut c::dentry, fs: &Fs) -> bool {
    // SAFETY: a dentry the VFS gave us, referenced
    unsafe { (*(*dentry).d_sb).s_fs_info.cast() == fs.as_ptr() }
}

/// A path the VFS passes an op - getattr's - ours, held for the call.
#[repr(transparent)]
pub struct Path(c::path);

impl Path {
    pub fn dentry(&self) -> Dentry<'_> {
        // SAFETY: held, its inode ours, as the FromRaw row has it
        unsafe { Dentry::borrow_raw(self.0.dentry) }
    }
}

/// A path looked up, from directory fd @dirfd: path_put() when it drops.
pub struct UserPath(c::path);

impl UserPath {
    pub fn at(dirfd: c_int, name: u64, flags: c_uint) -> Result<Self, BchError> {
        let mut p = MaybeUninit::uninit();
        ret_to_result_void(unsafe { c::user_path_at(dirfd, user_ptr(name), flags, p.as_mut_ptr()) })?;
        // SAFETY: user_path_at() filled it in
        Ok(UserPath(unsafe { p.assume_init() }))
    }

    pub fn dentry(&self) -> *mut c::dentry {
        self.0.dentry
    }

    pub fn on(&self, fs: &Fs) -> bool {
        on(self.0.dentry, fs)
    }
}

impl Drop for UserPath {
    fn drop(&mut self) {
        unsafe { c::path_put(&self.0) }
    }
}

/// A last component to create - @dentry, negative unless it exists - in
/// @path, the parent, locked, its mount writable.
pub struct Creating {
    pub path:   c::path,
    pub dentry: *mut c::dentry,
}

impl Creating {
    pub fn start(dirfd: c_int, name: u64) -> Result<Self, BchError> {
        let mut path = MaybeUninit::uninit();
        // SAFETY: the VFS's, filling in @path if it succeeds
        let dentry = unsafe {
            #[cfg(linux_ge = "6.18")]
            let d = c::start_creating_user_path(dirfd, user_ptr(name), path.as_mut_ptr(), 0);
            #[cfg(not(linux_ge = "6.18"))]
            let d = c::user_path_create(dirfd, user_ptr(name), path.as_mut_ptr(), 0);
            d
        };
        let dentry = errptr_to_result(NonNull::new(dentry).expect("a dentry, or an error"))?.as_ptr();
        Ok(Creating { path: unsafe { path.assume_init() }, dentry })
    }

    pub fn on(&self, fs: &Fs) -> bool {
        on(self.dentry, fs)
    }
}

impl Drop for Creating {
    fn drop(&mut self) {
        #[cfg(linux_ge = "6.18")]
        unsafe { c::end_creating_path(&self.path, self.dentry) }
        #[cfg(not(linux_ge = "6.18"))]
        unsafe { c::done_path_create(&mut self.path, self.dentry) }
    }
}

/// A last component to remove, @victim, in @path, the parent, locked, its
/// mount writable.
pub struct Removing {
    pub path:   c::path,
    pub victim: *mut c::dentry,
}

impl Removing {
    pub fn start(dirfd: c_int, name: u64) -> Result<Self, BchError> {
        let mut path = MaybeUninit::<c::path>::uninit();
        let victim = unsafe { start_removing(dirfd, user_ptr(name), path.as_mut_ptr()) };
        let victim = errptr_to_result(NonNull::new(victim).expect("a dentry, or an error"))?.as_ptr();
        Ok(Removing { path: unsafe { path.assume_init() }, victim })
    }

    pub fn on(&self, fs: &Fs) -> bool {
        on(self.victim, fs)
    }
}

impl Drop for Removing {
    fn drop(&mut self) {
        #[cfg(linux_ge = "6.18")]
        unsafe { c::end_removing_path(&self.path, self.victim) }
        #[cfg(not(linux_ge = "6.18"))]
        unsafe { c::done_path_create(&mut self.path, self.victim) }
    }
}

/// # Safety
/// @name a user pointer, @path for the parent: filled in, held, if it succeeds.
#[cfg(all(linux_ge = "6.18", not(linux_ge = "7.2")))]
unsafe fn start_removing(dirfd: c_int, name: *const c_char, path: *mut c::path) -> *mut c::dentry {
    unsafe { c::start_removing_user_path_at(dirfd, name, path) }
}

#[cfg(linux_ge = "7.2")]
unsafe fn start_removing(dirfd: c_int, name: *const c_char, path: *mut c::path) -> *mut c::dentry {
    unsafe {
        let mut victim = MaybeUninit::<c::path>::uninit();
        let ret = c::user_path_at(dirfd, name, 0, victim.as_mut_ptr());
        if ret != 0 {
            return crate::errcode::err_ptr(BchError::from_raw(-ret));
        }
        let victim = victim.assume_init();

        let ret = c::mnt_want_write(victim.mnt);
        if ret != 0 {
            c::path_put(&victim);
            return crate::errcode::err_ptr(BchError::from_raw(-ret));
        }

        let parent = c::dget_parent(victim.dentry);
        let child = c::start_removing_dentry(parent, victim.dentry);
        if errptr_to_result(NonNull::new(child).expect("a dentry, or an error")).is_err() {
            c::dput(parent);
            c::mnt_drop_write(victim.mnt);
            c::path_put(&victim);
            return child;
        }
        // The parent's path, with the victim's mount reference: child is a
        // reference of its own
        *path = c::path { mnt: victim.mnt, dentry: parent };
        c::dput(victim.dentry);
        child
    }
}

#[cfg(not(linux_ge = "6.18"))]
unsafe fn start_removing(dirfd: c_int, name: *const c_char, path: *mut c::path) -> *mut c::dentry {
    unsafe {
        let victim = c::user_path_locked_at(dirfd, name, path);
        if errptr_to_result(NonNull::new(victim).expect("a dentry, or an error")).is_err() {
            return victim;
        }
        let dir = (*(*path).dentry).d_inode;

        c::inode_unlock(dir);
        let mut ret = c::mnt_want_write((*path).mnt);
        c::inode_lock(dir);

        // A NULL pprev: unhashed, d_unhashed()
        if ret == 0 && ((*victim).d_hash.pprev.is_null() || (*victim).d_parent != (*path).dentry) {
            c::mnt_drop_write((*path).mnt);
            ret = -(c::ENOENT as c_int);
        }
        if ret != 0 {
            c::inode_unlock(dir);
            c::dput(victim);
            c::path_put(path);
            return crate::errcode::err_ptr(BchError::from_raw(-ret));
        }
        victim
    }
}
