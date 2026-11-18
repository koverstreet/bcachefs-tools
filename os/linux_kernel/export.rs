// SPDX-License-Identifier: GPL-2.0

//! The export operations: what exportfs, for NFS, has a filesystem do to turn
//! a file handle back into a dentry connected to the tree - a trait, Self
//! what s_fs_info points to, as the super ops are.
//!
//! A handle is the filesystem's own: u32s, which it writes into a HandleBuf
//! and reads back from a FileHandle as a Plain T - packed, as a handle's
//! u32s are only u32-aligned.

use core::ffi::c_int;
use core::mem::{align_of, size_of};

use crate::c;
use crate::errcode::BchError;
use crate::os::dentry::Dentry;
use crate::os::ops::{from_raw, FromRaw, SelfOf};
use crate::os::super_block::IsInode;
use crate::util::ffi::c_char;
use crate::util::vstructs::{cast_plain, plain_bytes, plain_bytes_mut, Plain};

/// A name as get_name returns it: at most NAME_MAX bytes, and a NUL.
pub type NameBuf = [u8; c::NAME_MAX as usize + 1];

from_raw! {
    /// get_name's, NAME_MAX + 1 bytes.
    &'a mut NameBuf = *mut c_char => |r| &mut *r.cast();
}

/// Where encode_fh() puts a handle: the VFS's u32s, and the length it's told
/// back.
pub struct HandleBuf<'a> {
    fh:  &'a mut [u32],
    len: &'a mut c_int,
}

impl HandleBuf<'_> {
    /// @v as a handle of type @ty: @ty - or FILEID_INVALID if there isn't
    /// room - the length told back the handle's either way.
    pub fn put<T: Plain>(&mut self, ty: u32, v: &T) -> c_int {
        const { assert!(size_of::<T>() % size_of::<u32>() == 0) };
        let n = size_of::<T>() / size_of::<u32>();
        *self.len = n as c_int;
        let Some(fh) = self.fh.get_mut(..n) else {
            return c::fid_type_FILEID_INVALID as c_int;
        };
        plain_bytes_mut(fh).copy_from_slice(plain_bytes(v));
        ty as c_int
    }
}

/// A handle exportfs hands over, of its type.
pub struct FileHandle<'a> {
    fh: &'a [u32],
    ty: u32,
}

impl FileHandle<'_> {
    /// # Safety
    /// @fid is exportfs's handle, @len u32s, for the call.
    unsafe fn from_raw<'a>(fid: *mut c::fid, len: c_int, ty: c_int) -> FileHandle<'a> {
        let fh = unsafe { core::slice::from_raw_parts(fid.cast::<u32>(), len.max(0) as usize) };
        FileHandle { fh, ty: ty as u32 }
    }

    /// It as a @T, if it's of type @ty, and a T's length.
    pub fn get<T: Plain + Copy>(&self, ty: u32) -> Option<T> {
        const { assert!(align_of::<T>() <= align_of::<u32>()) };
        let b = plain_bytes(self.fh);
        if self.ty != ty || b.len() != size_of::<T>() {
            return None;
        }
        cast_plain::<T>(b).copied()
    }
}

crate::vfs_ops! {
    pub(crate) trait ExportOps for c::export_operations, ExportVTable<T: ExportOps> = T;
    type Inode: IsInode<Super = Self>;

    /// @name NAME_MAX + 1 bytes.
    fn get_name(&self = child, parent: Dentry<'_, Self::Inode>, name: &mut NameBuf,
                child: Dentry<'_, Self::Inode>) -> Result<(), BchError>;
    /// As fh_to_dentry(), the directory @child is in's.
    fn get_parent(&self = child, child: Dentry<'_, Self::Inode>) -> *mut c::dentry;

    /// @inode's handle - with @parent's, if given - into @fh: HandleBuf::put()'s.
    fn encode_fh(&self = inode, inode: &Self::Inode, fh: &mut HandleBuf<'_>,
                 parent: Option<&Self::Inode>) -> c_int
        => extern (inode: *mut c::inode, fh: *mut u32, max_len: *mut c_int, parent: *mut c::inode) -> c_int
    {
        unsafe {
            let mut buf = HandleBuf {
                fh:  core::slice::from_raw_parts_mut(fh, (*max_len).max(0) as usize),
                len: &mut *max_len,
            };
            SelfOf::<T>::self_of(&inode).encode_fh(FromRaw::from_raw(inode), &mut buf, FromRaw::from_raw(parent))
        }
    };
    /// The dentry @fh names: NULL for one that isn't ours, an ERR_PTR, or
    /// d_obtain_alias()'s.
    fn fh_to_dentry(&self = sb, fh: FileHandle<'_>) -> *mut c::dentry
        => extern (sb: *mut c::super_block, fid: *mut c::fid, fh_len: c_int, fh_type: c_int) -> *mut c::dentry
    {
        unsafe { SelfOf::<T>::self_of(&sb).fh_to_dentry(FileHandle::from_raw(fid, fh_len, fh_type)) }
    };
    /// As fh_to_dentry(), the handle's parent's.
    fn fh_to_parent(&self = sb, fh: FileHandle<'_>) -> *mut c::dentry
        => extern (sb: *mut c::super_block, fid: *mut c::fid, fh_len: c_int, fh_type: c_int) -> *mut c::dentry
    {
        unsafe { SelfOf::<T>::self_of(&sb).fh_to_parent(FileHandle::from_raw(fid, fh_len, fh_type)) }
    };
}
