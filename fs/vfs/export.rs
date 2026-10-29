// SPDX-License-Identifier: GPL-2.0

//! The export operations: what exportfs, for NFS, has a filesystem do to turn
//! a file handle back into a dentry connected to the tree. They're a trait,
//! ExportOps, declared by vfs_ops! (ops.rs) as the super ops are: Self what
//! s_fs_info points to, Fs, and bch2_export_ops the table, s_export_op.
//!
//! A handle is a Fid - inode number, subvolume, generation - or, for a file
//! that isn't a directory, encoded with its parent, a FidWithParent: packed,
//! in the u32s exportfs hands over. Turned back, the inode is got from the
//! inode cache or the btree, and a generation that doesn't match is a file
//! since deleted and its number reused: ESTALE.
//!
//! get_name finds an inode's name in a directory. The inode's backpointer -
//! bi_dir, bi_dir_offset - is the dirent that made it, so one look finds it,
//! unless the backpointer is to another directory: a hardlink. Then the
//! directory's dirents are searched for one that names the inode.

use core::ffi::c_int;
use core::mem::size_of;
use core::ptr::{read_unaligned, write_unaligned};

use crate::btree::bkey::{pos, spos};
use crate::btree::iter::{lockrestart_do, BtreeIter, BtreeIterFlags};
use crate::c;
use crate::dirent::{self, Dirent};
use crate::errcode::{bch_errcode, BchError};
use crate::fs::Fs;
use crate::inode;
use crate::namei::subvol_inum_eq;
use crate::snapshots::subvolume;
use crate::util::os_str::OsStrExt;
use cstruct_macros::vtable;

use super::dentry::Dentry;
use super::inode::{InodeInfo, InodeRef};
use super::super_block::SuperBlock;

/// A name as get_name returns it: at most NAME_MAX bytes, and a NUL.
pub(crate) type NameBuf = [u8; c::NAME_MAX as usize + 1];

/// @d's name, as get_name returns it: cut at NAME_MAX.
fn copy_name(out: &mut NameBuf, d: Dirent<'_>) {
    let name = d.name().as_bytes();
    let len = name.len().min(c::NAME_MAX as usize);
    out[..len].copy_from_slice(&name[..len]);
    out[len] = 0;
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct Fid {
    inum:       u64,
    subvol:     u32,
    generation: u32,
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct FidWithParent {
    fid: Fid,
    dir: Fid,
}

/// @T's length in a handle: u32s.
const fn fh_len<T>() -> usize {
    size_of::<T>() / size_of::<u32>()
}

/// Handle @fid, if it's one of ours: of a type of ours, and that type's
/// length.
///
/// # Safety
/// @fid is exportfs's handle, @len u32s.
unsafe fn our_handle<'a>(fid: *mut c::fid, len: c_int, fh_type: c_int) -> Option<&'a [u32]> {
    let fh = unsafe { core::slice::from_raw_parts(fid.cast::<u32>(), len.max(0) as usize) };
    let len = match fh_type as u32 {
        c::fid_type_FILEID_BCACHEFS_WITHOUT_PARENT => fh_len::<Fid>(),
        c::fid_type_FILEID_BCACHEFS_WITH_PARENT    => fh_len::<FidWithParent>(),
        _ => return None,
    };
    (fh.len() == len).then_some(fh)
}

/// @v into @fh, as a handle of @fh_type: its type and length - or, if @fh is
/// too short, the length it needs.
fn encode<T>(fh: &mut [u32], v: T, fh_type: u32) -> Result<(c_int, usize), usize> {
    let len = fh_len::<T>();
    if fh.len() < len {
        return Err(len);
    }
    // SAFETY: in bounds; packed, so u32-aligned will do
    unsafe { write_unaligned(fh.as_mut_ptr().cast::<T>(), v) };
    Ok((fh_type as c_int, len))
}

impl InodeInfo {
    fn fid(&self) -> Fid {
        let inum = self.inum();
        Fid { inum: inum.inum, subvol: inum.subvol as u32, generation: self.ei_inode.bi_generation.get() }
    }
}

impl Fs {
    fn nfs_get_inode(&self, fid: Fid) -> Result<InodeRef, BchError> {
        let inode = InodeInfo::get(self, c::subvol_inum { subvol: fid.subvol as u64, inum: fid.inum })?;
        if inode.vfs().generation() != fid.generation {
            return Err(c::ESTALE.into());
        }
        Ok(inode)
    }
}

crate::vfs_ops! {
    pub(crate) trait ExportOps for c::export_operations, ExportVTable<T: ExportOps> = T;

    /// @name NAME_MAX + 1 bytes.
    fn get_name(&self = child, parent: Dentry<'_>, name: &mut NameBuf, child: Dentry<'_>)
        -> Result<(), BchError>;
    /// As fh_to_dentry(), the directory @child is in's.
    fn get_parent(&self = child, child: Dentry<'_>) -> *mut c::dentry;

    unsafe {
        /// @inode's handle - with @parent's, if given - into @fh, *@max_len
        /// u32s: its type, and *@max_len its length - or FILEID_INVALID, and
        /// the length it needs.
        fn encode_fh(&self = inode, inode: &InodeInfo, fh: *mut u32, max_len: *mut c_int,
                         parent: Option<&InodeInfo>) -> c_int;
        /// The dentry handle @fid, @fh_len u32s, of @fh_type names: NULL for
        /// one that isn't ours, an ERR_PTR, or d_obtain_alias()'s.
        fn fh_to_dentry(&self = sb, sb: &SuperBlock, fid: *mut c::fid, fh_len: c_int,
                            fh_type: c_int) -> *mut c::dentry;
        /// As fh_to_dentry(), the handle's parent's.
        fn fh_to_parent(&self = sb, sb: &SuperBlock, fid: *mut c::fid, fh_len: c_int,
                            fh_type: c_int) -> *mut c::dentry;
    }
}

#[vtable]
impl ExportOps for Fs {
    fn get_name(&self, parent: Dentry<'_>, name: &mut NameBuf, child: Dentry<'_>) -> Result<(), BchError> {
        let dir = parent.inode();
        if !dir.vfs().is_dir() {
            return self.throw(bch_errcode::BCH_ERR_EINVAL_get_name_not_dir);
        }

        let (dir_inum, inum) = (dir.inum(), child.inode().inum());
        let trans = crate::btree_trans!(self);
        lockrestart_do(&trans, |t| {
            let snapshot = subvolume::get_snapshot(t, dir_inum.subvol as u32)?;
            let bi = inode::find_by_inum_trans(t, inum, crate::c_function_name!())?;

            if bi.bi_dir == dir_inum.inum {
                let mut iter = BtreeIter::new(t, c::btree_id::dirents, pos(bi.bi_dir, bi.bi_dir_offset),
                                              BtreeIterFlags::empty());
                iter.set_snapshot(snapshot);

                let doesnt_match = || self.err(bch_errcode::BCH_ERR_ENOENT_dirent_doesnt_match_inode);
                let d = iter.peek_slot(t)?.and_then(Dirent::new).ok_or_else(doesnt_match)?;
                let target = dirent::read_target(t, dir_inum, d)?.ok_or_else(doesnt_match)?;
                if subvol_inum_eq(target, inum) {
                    copy_name(name, d);
                    return Ok(());
                }
            } else {
                let mut iter = BtreeIter::new(t, c::btree_id::dirents, pos(dir_inum.inum, 0),
                                              BtreeIterFlags::empty());
                iter.set_snapshot(snapshot);

                let found = iter.find_max_norestart(t, spos(dir_inum.inum, u64::MAX, u32::MAX), |_, k| {
                    let Some(d) = Dirent::new(k) else { return Ok(false) };
                    Ok(dirent::read_target(t, dir_inum, d)?.is_some_and(|target| subvol_inum_eq(target, inum)))
                })?;
                if let Some(k) = found {
                    copy_name(name, Dirent::new(k).expect("a dirent"));
                    return Ok(());
                }
            }

            Err(c::ENOENT.into())
        })
    }

    /// The inode's backpointer: its directory, in the parent subvolume if
    /// it's a subvolume's root.
    fn get_parent(&self, child: Dentry<'_>) -> *mut c::dentry {
        let inode = child.inode();
        let parent_subvol = inode.ei_inode.bi_parent_subvol.get();
        let parent = c::subvol_inum {
            subvol: if parent_subvol != 0 { parent_subvol as u64 } else { inode.inum().subvol },
            inum:   inode.ei_inode.bi_dir.get(),
        };
        Dentry::obtain_alias(InodeInfo::get(self, parent))
    }

    unsafe fn encode_fh(&self, inode: &InodeInfo, fh: *mut u32, max_len: *mut c_int,
                        parent: Option<&InodeInfo>) -> c_int {
        // SAFETY: the VFS's
        let fh = unsafe { core::slice::from_raw_parts_mut(fh, (*max_len).max(0) as usize) };
        let (fh_type, len) = match parent.filter(|_| !inode.vfs().is_dir()) {
            Some(dir) => encode(fh, FidWithParent { fid: inode.fid(), dir: dir.fid() },
                                c::fid_type_FILEID_BCACHEFS_WITH_PARENT),
            None      => encode(fh, inode.fid(), c::fid_type_FILEID_BCACHEFS_WITHOUT_PARENT),
        }.unwrap_or_else(|len| (c::fid_type_FILEID_INVALID as c_int, len));
        unsafe { *max_len = len as c_int };
        fh_type
    }

    unsafe fn fh_to_dentry(&self, _sb: &SuperBlock, fid: *mut c::fid, fh_len: c_int, fh_type: c_int)
        -> *mut c::dentry
    {
        // SAFETY: the VFS's
        let Some(fh) = (unsafe { our_handle(fid, fh_len, fh_type) }) else {
            return core::ptr::null_mut();
        };
        // SAFETY: a Fid's length at least; packed
        let fid = unsafe { read_unaligned(fh.as_ptr().cast::<Fid>()) };
        Dentry::obtain_alias(self.nfs_get_inode(fid))
    }

    unsafe fn fh_to_parent(&self, _sb: &SuperBlock, fid: *mut c::fid, fh_len: c_int, fh_type: c_int)
        -> *mut c::dentry
    {
        // SAFETY: the VFS's
        let fh = unsafe { our_handle(fid, fh_len, fh_type) };
        let Some(fh) = fh.filter(|_| fh_type as u32 == c::fid_type_FILEID_BCACHEFS_WITH_PARENT) else {
            return core::ptr::null_mut();
        };
        // SAFETY: a FidWithParent's length; packed
        let fid = unsafe { read_unaligned(fh.as_ptr().cast::<FidWithParent>()) };
        Dentry::obtain_alias(self.nfs_get_inode(fid.dir))
    }
}

pub(crate) static EXPORT_OPS: c::export_operations = ExportVTable::<Fs>::OPS;
