// SPDX-License-Identifier: GPL-2.0

//! The VFS's struct super_block, for Rust: the filesystem's, once it's
//! mounted - Fs::vfs_super().


use crate::c;
use crate::fs::Fs;
use crate::util::ffi::Opaque;

#[repr(transparent)]
pub struct SuperBlock(Opaque<c::super_block>);

impl Fs {
    /// The VFS superblock it's mounted as: None while it isn't.
    pub fn vfs_super(&self) -> Option<&SuperBlock> {
        // SAFETY: set at mount, live until unmount clears it
        unsafe { self.vfs_sb.cast::<SuperBlock>().as_ref() }
    }
}

impl SuperBlock {
    fn as_ptr(&self) -> *mut c::super_block {
        self.0.as_ptr()
    }

    pub fn dev(&self) -> c::dev_t {
        unsafe { (*self.as_ptr()).s_dev }
    }
}
