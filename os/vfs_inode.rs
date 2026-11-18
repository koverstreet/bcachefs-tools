// SPDX-License-Identifier: GPL-2.0

use core::marker::PhantomData;

use crate::c;
use crate::os::file::Any;
use crate::util::ffi::Opaque;

/// A struct inode of ours - one InodeInfo's - referenced while this is
/// borrowed, on a superblock whose s_fs_info is an S. Both builds lay it out;
/// only the kernel's has a VFS.
#[repr(transparent)]
pub struct VfsInode<S = Any>(pub(super) Opaque<c::inode>, PhantomData<fn() -> S>);
