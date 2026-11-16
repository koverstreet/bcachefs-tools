// SPDX-License-Identifier: GPL-2.0

//! The export operations: what exportfs, for NFS, has a filesystem do to turn
//! a file handle back into a dentry connected to the tree - a trait, Self
//! what s_fs_info points to, as the super ops are.

use core::ffi::c_int;

use crate::c;
use crate::errcode::BchError;
use crate::os::dentry::Dentry;
use crate::os::ops::from_raw;
use crate::os::super_block::{IsInode, SuperBlock};
use crate::util::ffi::c_char;

/// A name as get_name returns it: at most NAME_MAX bytes, and a NUL.
pub type NameBuf = [u8; c::NAME_MAX as usize + 1];

from_raw! {
    /// get_name's, NAME_MAX + 1 bytes.
    &'a mut NameBuf = *mut c_char => |r| &mut *r.cast();
}

crate::vfs_ops! {
    pub(crate) trait ExportOps for c::export_operations, ExportVTable<T: ExportOps> = T;
    type Inode: IsInode;

    /// @name NAME_MAX + 1 bytes.
    fn get_name(&self = child, parent: Dentry<'_, Self::Inode>, name: &mut NameBuf,
                child: Dentry<'_, Self::Inode>) -> Result<(), BchError>;
    /// As fh_to_dentry(), the directory @child is in's.
    fn get_parent(&self = child, child: Dentry<'_, Self::Inode>) -> *mut c::dentry;

    unsafe {
        /// @inode's handle - with @parent's, if given - into @fh, *@max_len
        /// u32s: its type, and *@max_len its length - or FILEID_INVALID, and
        /// the length it needs.
        fn encode_fh(&self = inode, inode: &Self::Inode, fh: *mut u32, max_len: *mut c_int,
                         parent: Option<&Self::Inode>) -> c_int;
        /// The dentry handle @fid, @fh_len u32s, of @fh_type names: NULL for
        /// one that isn't ours, an ERR_PTR, or d_obtain_alias()'s.
        fn fh_to_dentry(&self = sb, sb: &SuperBlock<Self>, fid: *mut c::fid, fh_len: c_int,
                            fh_type: c_int) -> *mut c::dentry;
        /// As fh_to_dentry(), the handle's parent's.
        fn fh_to_parent(&self = sb, sb: &SuperBlock<Self>, fid: *mut c::fid, fh_len: c_int,
                            fh_type: c_int) -> *mut c::dentry;
    }
}
