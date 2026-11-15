// SPDX-License-Identifier: GPL-2.0

//! Whether a path from userspace - os::path's - is on this filesystem.

use crate::c;
use crate::fs::Fs;
use crate::os::path::{Creating, Removing, UserPath};

/// Whether @dentry is on @fs.
fn on(dentry: *mut c::dentry, fs: &Fs) -> bool {
    // SAFETY: a dentry the VFS gave us, referenced
    unsafe { (*(*dentry).d_sb).s_fs_info.cast() == fs.as_ptr() }
}

impl UserPath {
    pub fn on(&self, fs: &Fs) -> bool {
        on(self.dentry(), fs)
    }
}

impl Creating {
    pub fn on(&self, fs: &Fs) -> bool {
        on(self.dentry, fs)
    }
}

impl Removing {
    pub fn on(&self, fs: &Fs) -> bool {
        on(self.victim, fs)
    }
}
