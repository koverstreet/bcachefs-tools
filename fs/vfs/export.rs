// SPDX-License-Identifier: GPL-2.0

//! The export operations: what exportfs, for NFS, has a filesystem do to turn
//! a file handle back into a dentry connected to the tree: os::export's
//! ExportOps, Fs's, and EXPORT_OPS the table, s_export_op.
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

use crate::btree::bkey::{pos, spos};
use crate::btree::iter::{lockrestart_do, BtreeIter, BtreeIterFlags};
use crate::c;
use crate::snapshots::subvolume::subvol_inum;
use crate::posix::dirent::{self, Dirent};
use crate::errcode::{Errcode, BchError};
use crate::fs::Fs;
use crate::posix::inode;
use crate::posix::namei::subvol_inum_eq;
use crate::types::CStatic;
use crate::snapshots::subvolume;
use crate::util::os_str::OsStrExt;
use cstruct_macros::{vtable, Plain};

use super::dentry::Dentry;
use crate::os::export::{ExportOps, ExportVTable, FileHandle, HandleBuf, NameBuf};
use super::inode::{InodeInfo, InodeRef};
use crate::format::Btree;

/// @d's name, as get_name returns it: cut at NAME_MAX.
fn copy_name(out: &mut NameBuf, d: Dirent<'_>) {
    let name = d.name().as_bytes();
    let len = name.len().min(c::NAME_MAX as usize);
    out[..len].copy_from_slice(&name[..len]);
    out[len] = 0;
}

#[repr(C, packed)]
#[derive(Clone, Copy, Plain)]
struct Fid {
    inum:       u64,
    subvol:     u32,
    generation: u32,
}

#[repr(C, packed)]
#[derive(Clone, Copy, Plain)]
struct FidWithParent {
    fid: Fid,
    dir: Fid,
}

const WITHOUT_PARENT: u32 = c::fid_type_FILEID_BCACHEFS_WITHOUT_PARENT;
const WITH_PARENT: u32    = c::fid_type_FILEID_BCACHEFS_WITH_PARENT;

impl InodeInfo {
    fn fid(&self) -> Fid {
        let inum = self.inum();
        Fid { inum: inum.inum, subvol: inum.subvol as u32, generation: self.ei_inode.bi_generation.get() }
    }
}

impl Fs {
    fn nfs_get_inode(&self, fid: Fid) -> Result<InodeRef, BchError> {
        let inode = InodeInfo::get(self, subvol_inum { subvol: fid.subvol as u64, inum: fid.inum })?;
        if inode.vfs().generation() != fid.generation {
            return Err(Errcode::ESTALE_nfs_get_inode_generation.into());
        }
        Ok(inode)
    }
}

#[vtable]
impl ExportOps for Fs {
    type Inode = InodeInfo;

    fn get_name(&self, parent: Dentry<'_>, name: &mut NameBuf, child: Dentry<'_>) -> Result<(), BchError> {
        let dir = parent.inode();
        if !dir.vfs().is_dir() {
            return self.throw(Errcode::EINVAL_get_name_not_dir);
        }

        let (dir_inum, inum) = (dir.inum(), child.inode().inum());
        let trans = crate::btree_trans!(self);
        lockrestart_do(&trans, |t| {
            let snapshot = subvolume::get_snapshot(t, dir_inum.subvol as u32)?;
            let bi = inode::find_by_inum_trans(t, inum, crate::here!())?;

            if bi.bi_dir == dir_inum.inum {
                let mut iter = BtreeIter::new(t, Btree::dirents, pos(bi.bi_dir, bi.bi_dir_offset),
                                              BtreeIterFlags::empty());
                iter.set_snapshot(snapshot);

                let doesnt_match = || self.err(Errcode::ENOENT_dirent_doesnt_match_inode);
                let d = iter.peek_slot(t)?.and_then(Dirent::new).ok_or_else(doesnt_match)?;
                let target = dirent::read_target(t, dir_inum, d)?.ok_or_else(doesnt_match)?;
                if subvol_inum_eq(target, inum) {
                    copy_name(name, d);
                    return Ok(());
                }
            } else {
                let mut iter = BtreeIter::new(t, Btree::dirents, pos(dir_inum.inum, 0),
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

            Err(Errcode::ENOENT_nfs_get_name.into())
        })
    }

    /// The inode's backpointer: its directory, in the parent subvolume if
    /// it's a subvolume's root.
    fn get_parent(&self, child: Dentry<'_>) -> *mut c::dentry {
        let inode = child.inode();
        let parent_subvol = inode.ei_inode.bi_parent_subvol.get();
        let parent = subvol_inum {
            subvol: if parent_subvol != 0 { parent_subvol as u64 } else { inode.inum().subvol },
            inum:   inode.ei_inode.bi_dir.get(),
        };
        Dentry::obtain_alias(InodeInfo::get(self, parent))
    }

    fn encode_fh(&self, inode: &InodeInfo, fh: &mut HandleBuf<'_>, parent: Option<&InodeInfo>) -> c_int {
        match parent.filter(|_| !inode.vfs().is_dir()) {
            Some(dir) => fh.put(WITH_PARENT, &FidWithParent { fid: inode.fid(), dir: dir.fid() }),
            None      => fh.put(WITHOUT_PARENT, &inode.fid()),
        }
    }

    /// Either handle names the inode first.
    fn fh_to_dentry(&self, fh: FileHandle<'_>) -> *mut c::dentry {
        let fid = fh.get::<Fid>(WITHOUT_PARENT).or_else(|| fh.get::<FidWithParent>(WITH_PARENT).map(|f| f.fid));
        let Some(fid) = fid else { return core::ptr::null_mut() };
        Dentry::obtain_alias(self.nfs_get_inode(fid))
    }

    fn fh_to_parent(&self, fh: FileHandle<'_>) -> *mut c::dentry {
        let Some(fid) = fh.get::<FidWithParent>(WITH_PARENT) else { return core::ptr::null_mut() };
        Dentry::obtain_alias(self.nfs_get_inode(fid.dir))
    }
}

// CONFIG_EXPORTFS_BLOCK_OPS's block_ops pointer makes export_operations !Sync
pub(crate) static EXPORT_OPS: CStatic<c::export_operations, Fs> = ExportVTable::<Fs>::STATIC;
