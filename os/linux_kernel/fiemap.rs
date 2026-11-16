// SPDX-License-Identifier: GPL-2.0

//! FIEMAP's request, as the VFS passes ->fiemap it: the range asked about,
//! and the user's buffer the mappings are copied out to.

use crate::c;
use crate::errcode::{ret_to_result, ret_to_result_void, BchError};
use crate::os::ops::from_raw;
use crate::os::vfs_inode::VfsInode;

pub struct Fiemap<'a>(&'a mut c::fiemap_extent_info);

from_raw! {
    Fiemap<'a> = *mut c::fiemap_extent_info => |r| Fiemap(&mut *r);
}

impl Fiemap<'_> {
    /// The request checked for @inode, and @len cut to what it may map:
    /// fiemap_prep().
    pub fn prep(&mut self, inode: &VfsInode, start: u64, len: &mut u64) -> Result<(), BchError> {
        ret_to_result_void(unsafe { c::fiemap_prep(inode.as_ptr(), self.0, start, len, 0) })
    }

    /// The next mapping, in bytes, copied out: whether that filled the
    /// buffer - fiemap_fill_next_extent().
    pub fn fill_next(&mut self, logical: u64, physical: u64, len: u64, flags: u32) -> Result<bool, BchError> {
        ret_to_result(unsafe { c::fiemap_fill_next_extent(self.0, logical, physical, len, flags) })
            .map(|full| full != 0)
    }
}
