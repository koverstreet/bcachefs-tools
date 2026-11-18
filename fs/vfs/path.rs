// SPDX-License-Identifier: GPL-2.0

//! A path from userspace - os::path's - on this filesystem: what it holds is
//! ours once its superblock is shown to be, and on() hands it out only then.

use crate::fs::Fs;
use super::dentry::Dentry;
use crate::os::path::{Creating, Removing, UserPath};

use super::inode::InodeInfo;

impl UserPath {
    /// Its inode - it's positive, held by the path - if it's on @fs.
    pub fn on<'a>(&'a self, fs: &Fs) -> Option<&'a InodeInfo> {
        Some(self.dentry().on(fs.vfs_super()?)?.inode())
    }
}

impl Creating {
    /// Its dentry, and the directory it's in - held, and locked - if it's on
    /// @fs.
    pub fn on<'a>(&'a self, fs: &Fs) -> Option<(Dentry<'a>, &'a InodeInfo)> {
        let sb = fs.vfs_super()?;
        Some((self.dentry().on(sb)?, self.dir().on(sb)?.inode()))
    }
}

impl Removing {
    /// Its victim, and the directory it's in - held, and locked - if it's on
    /// @fs.
    pub fn on<'a>(&'a self, fs: &Fs) -> Option<(Dentry<'a>, &'a InodeInfo)> {
        let sb = fs.vfs_super()?;
        Some((self.victim().on(sb)?, self.dir().on(sb)?.inode()))
    }
}
