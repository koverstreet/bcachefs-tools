// SPDX-License-Identifier: GPL-2.0

use core::marker::PhantomData;

use crate::c;
use crate::os::file::Any;
use crate::util::ffi::Opaque;
use crate::util::rcu::RcuDropMember;

/// A struct inode of ours - one InodeInfo's - referenced while this is
/// borrowed, on a superblock whose s_fs_info is an S, in state St: Live, or
/// New, its creator's alone. Both builds lay it out; only the kernel's has a
/// VFS.
///
/// Dropped and freed a grace period after it's out of RCU walkers' reach -
/// ->free_inode() is called from i_callback(), an RCU callback - and so is
/// whatever it's in: an RcuDropMember.
#[repr(transparent)]
pub struct VfsInode<S = Any, St = Live>(pub(super) Opaque<c::inode>, PhantomData<(fn() -> (S, St), RcuDropMember)>);

// The kernel's, shared by every task with it open or in a VFS call: its
// fields are C's, read and written as the VFS does, under its locks
unsafe impl<S, St> Send for VfsInode<S, St> {}
unsafe impl<S, St> Sync for VfsInode<S, St> {}

/// An inode anyone may hold a reference to: published.
pub enum Live {}

/// An inode only its creator has, not yet published - NewInode's: it has no
/// reference to take.
pub enum New {}
