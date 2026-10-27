// SPDX-License-Identifier: GPL-2.0

//! The namespace operations as the VFS makes them: each takes the update
//! locks of the inodes it changes, runs its transaction - fs/namei.rs's - and
//! brings the VFS inodes up to date with what it wrote. Instantiating the
//! dentries a link or create makes is still C's, in the inode ops.
//!
//! Lookup writes nothing, unless a dirent and its inode disagree, which it
//! repairs. The inodes it returns come from the inode cache, which is still
//! fs.c's.

use core::ffi::{c_int, c_uint};

use crate::btree::bkey::BkeyV;
use crate::btree::iter::{commit_do, lockrestart_do, BtreeIter, BtreeIterFlags, CommitFlags, CommitOpts,
                         TransAttempt};
use crate::c;
use crate::dirent::{self, Dirent};
use crate::errcode::{bch_errcode, ret_to_c, ret_to_class, BchError};
use crate::init::passes;
use crate::inode;
use crate::namei;
use crate::quota;
use crate::snapshots::subvolume;
use crate::str_hash;
use crate::util::kernel::{current_fsgid, current_fsuid};
use crate::util::log::LogMsg;
use crate::util::os_str::{qstr_name, OsStr};
use cstruct_macros::c_export;

use super::dentry::Dentry;
use super::inode::{InodeInfo, InodeRef, ATTR_CTIME, ATTR_MTIME, ATTR_SIZE};

const DIR_ENTRIES_CHANGED: u32 = ATTR_MTIME | ATTR_CTIME | ATTR_SIZE;

impl InodeInfo {
    /// The inode @name in this directory names, through the inode cache - @hash
    /// is this directory's. ENOENT if there's no such name, or it's a subvolume
    /// this directory doesn't list.
    pub fn lookup(&self, hash: &c::bch_hash_info, name: &OsStr) -> Result<InodeRef, BchError> {
        let dir = self.inum();
        let trans = crate::btree_trans!(self.fs());

        lockrestart_do(&trans, |t| {
            let mut iter = BtreeIter::uninit();
            let k = dirent::lookup_key(t, &mut iter, dir, hash, name, BtreeIterFlags::empty())?;
            let inum = dirent::lookup_target(t, dir, k)?;

            if let Some(inode) = InodeInfo::cache_find(t, inum)? {
                return Ok(inode);
            }

            // Peeked again for the iterator and the dirent together, as
            // check_dirent_target() takes them
            let (iter, k) = iter.peek_slot_with(t)?.expect("the dirent lookup_key() found");
            let d = Dirent::new(k).expect("a dirent");

            // check_dirent_target()'s repairs are committed before the insert,
            // past which no restart can be taken
            let mut subvol = c::Subvolume::default();
            let ret = (|| {
                subvol = subvolume::get(t, inum.subvol as u32, true)?;
                let mut bi = inode::find_by_inum_snapshot(t, inum.inum, subvol.snapshot.get(),
                                                          BtreeIterFlags::CACHED)?;
                namei::check_dirent_target(t, iter, d, &mut bi, false)?;
                t.commit(None, CommitFlags::NO_ENOSPC)?;
                InodeInfo::cache_insert(t, inum, &mut bi, &mut subvol)
            })();

            match ret {
                // The dirent stays: check_inodes may find an inode that points
                // back to it
                Err(e) if e.matches(c::ENOENT) => {
                    dirent_to_missing_inode(t, d, dir, inum, subvol.snapshot.get(), &e)?;
                    Err(e)
                }
                ret => ret,
            }
        })
    }

    /// Only this inode's update lock, as C took: the directory's VFS inode is
    /// updated under its btree position's lock, which serializes that.
    pub fn link(&self, dir: &InodeInfo, name: &OsStr) -> Result<(), BchError> {
        let _lock = self.update_lock();
        let trans = crate::btree_trans!(self.fs());
        let [mut dir_u, mut inode_u] = [c::bch_inode_unpacked::default(); 2];

        commit_do(&trans, None, CommitOpts::new(), |t| {
            namei::link_trans(t, dir.inum(), &mut dir_u, self.inum(), &mut inode_u, name)
        })?;

        dir.update_after_write(&trans, &dir_u, DIR_ENTRIES_CHANGED);
        self.update_after_write(&trans, &inode_u, ATTR_CTIME);
        Ok(())
    }

    /// @deleting_subvol: @dentry is a subvolume's root, the subvolume going.
    pub fn unlink(&self, dentry: Dentry<'_>, deleting_subvol: bool) -> Result<(), BchError> {
        let inode = dentry.inode();
        {
            let _locks = InodeInfo::update_lock_all([self, inode]);
            let trans = crate::btree_trans!(self.fs());
            let [mut dir_u, mut inode_u] = [c::bch_inode_unpacked::default(); 2];

            commit_do(&trans, None, CommitFlags::NO_ENOSPC, |t| {
                namei::unlink_trans(t, self.inum(), &mut dir_u, inode.inum(), &mut inode_u,
                                    dentry.name(), deleting_subvol)
            })?;

            self.update_after_write(&trans, &dir_u, DIR_ENTRIES_CHANGED);
            inode.update_after_write(&trans, &inode_u, ATTR_CTIME);
        }

        /*
         * d_invalidate() waits for other threads killing child dentries, and
         * evicting a deleted child takes btree locks: called with none of ours
         * held, as vfs_rmdir() does shrink_dcache_parent()
         */
        if self.vfs().flag(c::S_CASEFOLD) {
            dentry.invalidate();
        }
        Ok(())
    }

    /// Rename @src_dentry, in this directory, to @dst_dentry in @dst_dir:
    pub fn rename(&self, src_dentry: Dentry<'_>, dst_dir: &InodeInfo, dst_dentry: Dentry<'_>,
                  flags: u32) -> Result<(), BchError> {
        use c::bch_rename_mode::*;

        let fs = self.fs();
        let src_dir = self;
        let src_inode = src_dentry.inode();
        let dst_inode = dst_dentry.inode_opt();
        let mode = if flags & c::RENAME_EXCHANGE != 0 {
            BCH_RENAME_EXCHANGE
        } else if dst_inode.is_some() {
            BCH_RENAME_OVERWRITE
        } else {
            BCH_RENAME
        };

        if flags & !(c::RENAME_NOREPLACE | c::RENAME_EXCHANGE | c::RENAME_WHITEOUT) != 0 {
            return fs.throw(bch_errcode::BCH_ERR_EINVAL_rename_bad_flags);
        }

        if mode == BCH_RENAME_OVERWRITE {
            src_inode.vfs().write_and_wait()?;
        }

        // Without a dst_inode, src_inode in its place: locked once all the same
        let locks = InodeInfo::update_lock_all([src_dir, dst_dir, src_inode,
                                                dst_inode.unwrap_or(src_inode)]);
        let ret = (|| {
            let trans = crate::btree_trans!(fs);

            lockrestart_do(&trans, |t| {
                subvolume::is_ro_trans(t, src_dir.inum().subvol as u32)?;
                subvolume::is_ro_trans(t, dst_dir.inum().subvol as u32)?;
                Ok(())
            })?;

            // An inode moved into a directory whose project it inherits is
            // charged to that project from here: put back below to the project
            // the inode was written with, whatever happens
            locks.get(src_inode).inherit_project(dst_dir)?;
            if mode == BCH_RENAME_EXCHANGE {
                let dst_inode = dst_inode.expect("an exchange with nothing to exchange");
                locks.get(dst_inode).inherit_project(src_dir)?;
            }

            let [mut src_dir_u, mut dst_dir_u, mut src_inode_u, mut dst_inode_u] =
                [c::bch_inode_unpacked::default(); 4];
            // Outside the attempts: the propagates they start are finished after
            // the commit
            let mut src_opt_change = c::inode_opt_change::new();
            let mut dst_opt_change = c::inode_opt_change::new();

            lockrestart_do(&trans, |t| {
                namei::rename_trans(t, src_dir.inum(), &mut src_dir_u, dst_dir.inum(), &mut dst_dir_u,
                                    &mut src_inode_u, &mut dst_inode_u,
                                    src_dentry.name(), dst_dentry.name(), mode,
                                    &mut src_opt_change, &mut dst_opt_change)?;

                if flags & c::RENAME_WHITEOUT != 0 {
                    // In the transaction's memory, as C had it: off the stack
                    let w = t.alloc_zeroed::<c::bch_inode_unpacked>()?;
                    inode::init_early(fs, w);

                    let mut new_subvol = c::Subvolume::default();
                    let uid = src_dir.vfs().uid_of(current_fsuid());
                    let gid = src_dir.vfs().gid_of(current_fsgid());
                    namei::create_trans(t, src_dir.inum(), &mut src_dir_u, w, &mut new_subvol,
                                        Some(src_dentry.name()), uid, gid,
                                        (c::S_IFCHR | c::WHITEOUT_MODE) as c::umode_t, 0,
                                        None, None, c::subvol_inum::default(), 0)?;
                    if !src_dir.flag(c::bch_inode_info_flags::EI_INODE_SNAPSHOT) {
                        quota::acct(fs, quota::qid(w), c::quota_counters::Q_INO.0 as usize, 1,
                                    c::quota_acct_mode::KEY_TYPE_QUOTA_PREALLOC)?;
                    }
                }

                t.commit(None, CommitOpts::new())
            })?;

            assert_eq!(src_inode.inum().inum, src_inode_u.bi_inum, "renamed the wrong inode");
            if let Some(dst_inode) = dst_inode {
                assert_eq!(dst_inode.inum().inum, dst_inode_u.bi_inum, "renamed over the wrong inode");
            }

            src_dir.update_after_write(&trans, &src_dir_u, DIR_ENTRIES_CHANGED);
            if !core::ptr::eq(src_dir, dst_dir) {
                dst_dir.update_after_write(&trans, &dst_dir_u, DIR_ENTRIES_CHANGED);
            }
            src_inode.update_after_write(&trans, &src_inode_u, ATTR_CTIME);
            if let Some(dst_inode) = dst_inode {
                dst_inode.update_after_write(&trans, &dst_inode_u, ATTR_CTIME);
            }

            src_opt_change.finish(&trans)?;
            dst_opt_change.finish(&trans)
        })();

        // Whether it happened or not, the project quota is the inode's, as
        // written:
        let prj = 1 << c::quota_types::QTYP_PRJ.0;
        for inode in [Some(src_inode), dst_inode].into_iter().flatten() {
            let _ = locks.get(inode).quota_transfer(quota::qid(&inode.inode()), prj,
                                                    c::quota_acct_mode::KEY_TYPE_QUOTA_NOCHECK);
        }
        ret
    }
}

/// Dirent @d, in @dir, points at @inum, which isn't there: @err, looking for
/// it in @snapshot. Not a reason to take the filesystem read-only - logged, and
/// check_dirents scheduled.
fn dirent_to_missing_inode(t: &TransAttempt<'_, '_>, d: Dirent<'_>, dir: c::subvol_inum,
                           inum: c::subvol_inum, snapshot: u32, err: &BchError)
    -> Result<(), BchError>
{
    let fs = t.fs();
    let mut msg = LogMsg::new(fs);

    write!(msg, "dirent to missing inode: ({err})\n{}\n in: ", d.k().to_text(fs));
    namei::inum_to_path(t, dir, &mut msg)?;
    write!(msg, "\ndir subvol {} inum subvol {} snapshot {snapshot}\n", dir.subvol, inum.subvol);

    // Scheduling the pass takes sb_lock, and may write the superblock
    t.unlock();
    passes::run_explicit(fs, &mut msg, c::bch_recovery_pass::BCH_RECOVERY_PASS_check_dirents,
                         c::bch_run_recovery_pass_flags::empty())
}

/// The directory inode ops' lookup. Any error is a negative dentry, as C had
/// it.
///
/// # Safety
/// The VFS's, ours: the directory referenced and locked, and the dentry in it
/// referenced, for the call.
#[c_export]
pub unsafe fn bch2_lookup(vdir: *mut c::inode, dentry: *mut c::dentry, _flags: c_uint) -> *mut c::dentry {
    let dir = unsafe { InodeInfo::from_inode(vdir) };
    let dentry = unsafe { Dentry::borrow_raw(dentry) };
    let fs = dir.fs();

    let Ok(hash) = crate::bch_err_fn!(fs, str_hash::hash_info_init(fs, &dir.inode())) else {
        return dentry.splice_alias(None);
    };
    let inode = dir.lookup(&hash, dentry.name()).ok();

    // No negative dentries in a casefolded directory: one for "blAH" would
    // still say it isn't there once "BLAH" is created - until the VFS has a
    // way to invalidate them
    if inode.is_none() && dir.vfs().flag(c::S_CASEFOLD) {
        return core::ptr::null_mut();
    }
    if let Some(inode) = &inode {
        dentry.set_casefold_ops(inode.vfs());
    }
    dentry.splice_alias(inode)
}

/// For C's link, until the inode ops are Rust.
///
/// # Safety
/// @name valid for the call.
#[c_export]
pub unsafe fn __bch2_link(inode: &c::bch_inode_info, dir: &c::bch_inode_info, name: &c::qstr) -> c_int {
    ret_to_c(inode.link(dir, unsafe { qstr_name(name) }))
}

/// The directory inode ops' rename.
///
/// # Safety
/// The VFS's, ours: the directories referenced and locked, the dentries
/// referenced, for the call.
#[c_export]
pub unsafe fn bch2_rename2(_idmap: *mut c::mnt_idmap,
                           src_vdir: *mut c::inode, src_dentry: *mut c::dentry,
                           dst_vdir: *mut c::inode, dst_dentry: *mut c::dentry,
                           flags: c_uint) -> c_int {
    let (src_dir, dst_dir) = unsafe { (InodeInfo::from_inode(src_vdir), InodeInfo::from_inode(dst_vdir)) };
    let (src_dentry, dst_dentry) = unsafe { (Dentry::borrow_raw(src_dentry), Dentry::borrow_raw(dst_dentry)) };
    ret_to_class(src_dir.rename(src_dentry, dst_dir, dst_dentry, flags))
}

/// For C's unlink and rmdir, and subvolume deletion, until they're Rust.
///
/// # Safety
/// @vdir is one of our directories and @dentry one of our dentries in it,
/// both referenced, and @vdir locked, for the call.
#[c_export]
pub unsafe fn __bch2_unlink(vdir: *mut c::inode, dentry: *mut c::dentry, deleting_subvol: bool) -> c_int {
    let dir = unsafe { InodeInfo::from_inode(vdir) };
    ret_to_c(dir.unlink(unsafe { Dentry::borrow_raw(dentry) }, deleting_subvol))
}
