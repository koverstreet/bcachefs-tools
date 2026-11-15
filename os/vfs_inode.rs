// SPDX-License-Identifier: GPL-2.0

use crate::c;
use crate::util::ffi::Opaque;

/// A struct inode of ours - one InodeInfo's - referenced while this is
/// borrowed. Both builds lay it out; only the kernel's has a VFS.
#[repr(transparent)]
pub struct VfsInode(pub(super) Opaque<c::inode>);
