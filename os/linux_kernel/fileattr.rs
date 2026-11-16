// SPDX-License-Identifier: GPL-2.0

//! The VFS's struct file_kattr: an inode's flags, as FS_IOC_GETFLAGS and
//! FS_IOC_FSGETXATTR report them and their setters change them.

use crate::c;

/// @fa's flags, both forms of them, from FS_XFLAG_* @xflags:
/// fileattr_fill_xflags().
pub fn fill_xflags(fa: &mut c::file_kattr, xflags: u32) {
    unsafe { c::fileattr_fill_xflags(fa, xflags) }
}
