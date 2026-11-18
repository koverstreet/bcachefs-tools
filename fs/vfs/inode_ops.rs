// SPDX-License-Identifier: GPL-2.0

//! bcachefs's inode operations, os::inode_ops's InodeOps.
//!
//! There are four tables - a directory's, a regular file's, a symlink's, and
//! everything else's - so four zero-sized types implement it. The ops every
//! table has are InodeInfo's, InodeAttrs (attr.rs); the rest are each
//! table's. A directory's, DirOps, are the namespace operations, namei.rs's.

use crate::c;
use crate::errcode::BchError;
use crate::util::ffi::c_char;
use crate::types::CStatic;
use cstruct_macros::vtable;

use super::dentry::Dentry;
use crate::os::fiemap::Fiemap;
use crate::os::inode_ops::{InodeOps, InodeVTable};
use super::inode::InodeInfo;
use super::namei::DirOps;

pub(super) struct FileInodeOps;
pub(super) struct SymlinkOps;
pub(super) struct SpecialOps;

#[vtable]
impl InodeOps for FileInodeOps {
    type Inode = InodeInfo;

    fn fiemap(inode: &InodeInfo, info: Fiemap<'_>, start: u64, len: u64) -> Result<(), BchError> {
        super::fiemap::fiemap(inode, info, start, len)
    }
}

#[vtable]
impl InodeOps for SymlinkOps {
    type Inode = InodeInfo;

    /// The target, in the page cache: symlink() wrote it there.
    fn get_link(dentry: Option<Dentry<'_>>, inode: &InodeInfo, done: &mut c::delayed_call)
        -> *const c_char
    {
        inode.vfs().page_get_link(dentry, done)
    }
}

#[vtable]
impl InodeOps for SpecialOps {
    type Inode = InodeInfo;
}

// i_op, as NewInode::init() sets it:
pub(super) static DIR_INODE_OPS: CStatic<c::inode_operations, DirOps> = InodeVTable::<DirOps>::STATIC;
pub(super) static FILE_INODE_OPS: CStatic<c::inode_operations, FileInodeOps> = InodeVTable::<FileInodeOps>::STATIC;
pub(super) static SYMLINK_INODE_OPS: CStatic<c::inode_operations, SymlinkOps> = InodeVTable::<SymlinkOps>::STATIC;
pub(super) static SPECIAL_INODE_OPS: CStatic<c::inode_operations, SpecialOps> = InodeVTable::<SpecialOps>::STATIC;
