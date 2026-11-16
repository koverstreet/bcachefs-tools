// SPDX-License-Identifier: GPL-2.0

//! A path from userspace - os::path's - on this filesystem: what it holds is
//! ours once its superblock is shown to be, and on() hands it out only then.

use crate::c;
use crate::fs::Fs;
use crate::os::dentry::Dentry;
use crate::os::path::{Creating, Removing, UserPath};

use super::inode::InodeInfo;

/// Whether @dentry is on @fs: then it, its inode and its parent's are ours.
fn on(dentry: *mut c::dentry, fs: &Fs) -> bool {
    // SAFETY: a dentry the VFS gave us, referenced
    unsafe { (*(*dentry).d_sb).s_fs_info.cast() == fs.as_ptr() }
}

impl UserPath {
    /// Its inode - it's positive, held by the path - if it's on @fs.
    pub fn on<'a>(&'a self, fs: &Fs) -> Option<&'a InodeInfo> {
        on(self.dentry(), fs).then(|| unsafe { InodeInfo::from_inode((*self.dentry()).d_inode) })
    }
}

impl Creating {
    /// Its dentry, and the directory it's in - held, and locked - if it's on
    /// @fs.
    pub fn on<'a>(&'a self, fs: &Fs) -> Option<(Dentry<'a>, &'a InodeInfo)> {
        on(self.dentry, fs).then(|| unsafe {
            (Dentry::borrow_raw(self.dentry), InodeInfo::from_inode((*self.path.dentry).d_inode))
        })
    }
}

impl Removing {
    /// Its victim, and the directory it's in - held, and locked - if it's on
    /// @fs.
    pub fn on<'a>(&'a self, fs: &Fs) -> Option<(Dentry<'a>, &'a InodeInfo)> {
        on(self.victim, fs).then(|| unsafe {
            (Dentry::borrow_raw(self.victim), InodeInfo::from_inode((*self.path.dentry).d_inode))
        })
    }
}
