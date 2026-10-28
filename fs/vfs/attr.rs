// SPDX-License-Identifier: GPL-2.0

//! An inode's attributes, as the VFS reads and changes them: getattr, and
//! setattr - all but a size change, which is a truncate, and C's still.
//!
//! getattr reads the VFS inode - its times are the VFS's, newer than the
//! btree's under lazytime - and of ei_inode only what the VFS inode doesn't
//! have: the creation time and the flags stat(2) reports.
//!
//! The iattr has uids and gids as the mount's users see them (vfsuid_t,
//! vfsgid_t); the inode keeps them in its filesystem's user namespace. So a
//! new one goes through the mount's idmap to a kuid, then to the number stored
//! - from_vfsuid(), from_kuid() - as the VFS's own setattr_copy() does.
//!
//! The quota follows the owner: a chown is a quota transfer, made before the
//! transaction - it can fail, and then nothing changes - and undone if the
//! transaction fails.
//!
//! fileattr is the inode flags FS_IOC_GETFLAGS and FS_IOC_FSGETXATTR show,
//! each its own bit there - FLAGS - the project, and casefolding. Setting
//! them checks the flags against the inode as the btree has it, not i_flags:
//! the btree's locks are what keep two setters apart.

use core::ffi::c_int;

use crate::acl::{self, AclType};
use crate::btree::iter::{lockrestart_do, BtreeIter, BtreeIterFlags, CommitFlags, TransAttempt};
use crate::c;
use crate::errcode::{bch_errcode, ret_to_c, ret_to_result_void, BchError};
use crate::inode;
use crate::inode_opts::InodeOpt;
use crate::snapshots::subvolume;
use cstruct_macros::{c_export, c_extern};

use super::dentry::Dentry;
use super::iattr::Iattr;
use super::idmap::MntIdmap;
use super::inode::{InodeInfo, ATTR_CTIME, ATTR_SIZE};

c_extern! {
    // io.c's, until it's Rust:
    fn bchfs_truncate(idmap: *mut c::mnt_idmap, inode: *mut c::bch_inode_info,
                      attr: *mut c::iattr) -> c_int;
}

use c::bch_inode_flags as F;

/// Each flag's bit in FS_IOC_GETFLAGS's flags, and in FS_IOC_FSGETXATTR's.
const FLAGS: [(F, u32, u32); 5] = [
    (F::BCH_INODE_sync,      c::FS_SYNC_FL,      c::FS_XFLAG_SYNC),
    (F::BCH_INODE_immutable, c::FS_IMMUTABLE_FL, c::FS_XFLAG_IMMUTABLE),
    (F::BCH_INODE_append,    c::FS_APPEND_FL,    c::FS_XFLAG_APPEND),
    (F::BCH_INODE_nodump,    c::FS_NODUMP_FL,    c::FS_XFLAG_NODUMP),
    (F::BCH_INODE_noatime,   c::FS_NOATIME_FL,   c::FS_XFLAG_NOATIME),
];

/// The inode flags of @fl, each flag's bit there given by @bit - and those
/// bits cleared from @fl, leaving any it has no flag for.
fn flags_from(fl: &mut u32, bit: impl Fn(&(F, u32, u32)) -> u32) -> u32 {
    let mut flags = 0;
    for f in &FLAGS {
        if *fl & bit(f) != 0 {
            *fl &= !bit(f);
            flags |= f.0.bits();
        }
    }
    flags
}

/// What a fileattr_set() changes: the inode flags of @mask to @flags, and
/// the project and casefolding if they're Some - the project stored +1.
#[derive(Default)]
struct FileattrSet {
    mask:     u32,
    flags:    u32,
    project:  Option<u32>,
    casefold: Option<bool>,
}

impl FileattrSet {
    fn apply(&self, t: &TransAttempt<'_, '_>, inode: &InodeInfo, bi: &mut c::bch_inode_unpacked)
        -> Result<(), BchError>
    {
        let fs = t.fs();
        let file_type = bi.bi_mode as u32 & c::S_IFMT;
        let others_may = (F::BCH_INODE_nodump | F::BCH_INODE_noatime).bits();
        if file_type != c::S_IFREG && file_type != c::S_IFDIR && self.flags & !others_may != 0 {
            return fs.throw(bch_errcode::BCH_ERR_EINVAL_setattr_bad_file_type);
        }

        if let Some(casefold) = self.casefold.filter(|&cf| cf != bi.casefold(fs)) {
            inode::set_casefold(t, inode.inum(), bi, casefold as u32)?;
        }

        if let Some(project) = self.project {
            bi.bi_project = project;
            InodeOpt::of(c::inode_opt_id::Inode_opt_project).set_own(bi, true);
        }

        bi.bi_flags = bi.bi_flags & !self.mask | self.flags;
        bi.bi_ctime = fs.timespec_to_time(inode.vfs().current_time()) as u64;
        Ok(())
    }
}

impl InodeInfo {
    pub(crate) fn getattr(&self, idmap: MntIdmap<'_>, stat: &mut c::kstat, request_mask: u32) {
        let fs = self.fs();
        let v = self.vfs();
        let block_bytes = fs.block_bytes() as u32;

        stat.dev     = v.sb().dev();
        stat.ino     = self.inum().inum;
        stat.mode    = v.mode();
        stat.nlink   = v.nlink();
        (stat.uid, stat.gid) = idmap.owner(v);
        stat.rdev    = v.rdev();
        stat.size    = v.size();
        if stat.size == 0 && v.is_dir() {
            stat.size = block_bytes as i64;
        }
        stat.atime   = v.atime();
        stat.mtime   = v.mtime();
        stat.ctime   = v.ctime();
        stat.blksize = block_bytes;
        stat.blocks  = v.blocks();

        stat.subvol = self.inum().subvol;
        stat.result_mask |= c::STATX_SUBVOL;

        if request_mask & c::STATX_DIOALIGN != 0 && v.is_reg() {
            stat.result_mask |= c::STATX_DIOALIGN;
            // Not right: it should be the superblock's, and checked against
            // the open devices' alignment
            stat.dio_mem_align    = 512;
            stat.dio_offset_align = block_bytes;
        }

        if request_mask & c::STATX_BTIME != 0 {
            stat.result_mask |= c::STATX_BTIME;
            stat.btime = fs.time_to_timespec(self.ei_inode.bi_otime.get() as i64);
        }

        let flags = self.ei_inode.bi_flags.get();
        for (flag, attr) in [(F::BCH_INODE_immutable, c::STATX_ATTR_IMMUTABLE),
                             (F::BCH_INODE_append,    c::STATX_ATTR_APPEND),
                             (F::BCH_INODE_nodump,    c::STATX_ATTR_NODUMP)] {
            if flags & flag.bits() != 0 {
                stat.attributes |= attr as u64;
            }
            stat.attributes_mask |= attr as u64;
        }
    }

    pub(crate) fn fileattr_get(&self, fa: &mut c::file_kattr) {
        let bi = self.inode();
        let xflags = FLAGS.iter().filter(|f| bi.flag(f.0)).fold(0, |x, f| x | f.2);

        unsafe { c::fileattr_fill_xflags(fa, xflags) };
        if InodeOpt::of(c::inode_opt_id::Inode_opt_project).is_own(&bi) {
            fa.fsx_xflags |= c::FS_XFLAG_PROJINHERIT;
        }
        if bi.casefold(self.fs()) {
            fa.flags |= c::FS_CASEFOLD_FL;
        }
        fa.fsx_projid = self.ei_quota.lock().ei_qid.q[c::quota_types::QTYP_PRJ.0 as usize];
    }

    /// @dentry is this inode's.
    pub(crate) fn fileattr_set(&self, dentry: Dentry<'_>, fa: &mut c::file_kattr)
        -> Result<(), BchError>
    {
        let fs = self.fs();
        let all = FLAGS.iter().fold(0, |m, f| m | f.0.bits());
        let mut s = FileattrSet::default();

        if fa.fsx_valid() {
            fa.fsx_xflags &= !c::FS_XFLAG_PROJINHERIT;
            s.mask = all;
            s.flags |= flags_from(&mut fa.fsx_xflags, |f| f.2);
            if fa.fsx_xflags != 0 {
                return fs.throw(bch_errcode::BCH_ERR_unsupported_fsx_flag);
            }

            if fa.fsx_projid >= u32::MAX {
                return fs.throw(bch_errcode::BCH_ERR_projid_too_big);
            }
            let project = self.ei_inode.bi_project.get();
            if (project != 0 || fa.fsx_projid != 0) && project != fa.fsx_projid + 1 {
                s.project = Some(fa.fsx_projid + 1);
            }
        }

        if fa.flags_valid() {
            s.mask = all;
            s.casefold = Some(fa.flags & c::FS_CASEFOLD_FL != 0);
            fa.flags &= !c::FS_CASEFOLD_FL;
            s.flags |= flags_from(&mut fa.flags, |f| f.1);
            if fa.flags != 0 {
                return fs.throw(bch_errcode::BCH_ERR_unsupported_fa_flag);
            }
        }

        {
            let lock = self.update_lock();
            subvolume::is_ro(fs, self.inum().subvol as u32)?;
            if s.project.is_some() {
                lock.set_projid(fa.fsx_projid)?;
            }
            lock.write_inode(ATTR_CTIME, |t, bi| s.apply(t, self, bi))?;
        }

        if s.casefold.is_some() {
            dentry.casefold_changed();
        }
        Ok(())
    }

    /// What @attr changes, into @bi: @uid as stored, @kgid as the
    /// filesystem has it.
    fn setattr_copy(&self, idmap: MntIdmap<'_>, attr: &Iattr, uid: Option<u32>,
                    kgid: Option<c::kgid_t>, bi: &mut c::bch_inode_unpacked) {
        let fs = self.fs();
        let v = self.vfs();

        if let Some(uid) = uid {
            bi.bi_uid = uid;
        }
        if let Some(gid) = kgid {
            bi.bi_gid = v.gid_of(gid);
        }

        if let Some(size) = attr.size() {
            bi.bi_size = size as u64;
        }

        for (ts, t) in [(attr.atime(), &mut bi.bi_atime), (attr.mtime(), &mut bi.bi_mtime),
                        (attr.ctime(), &mut bi.bi_ctime)] {
            if let Some(ts) = ts {
                *t = fs.timespec_to_time(ts) as u64;
            }
        }

        if let Some(mut mode) = attr.mode() {
            // setgid stays only if the caller is in the group the file will
            // have, or may act as if it were
            let gid = kgid.unwrap_or_else(|| v.kgid());
            if !idmap.in_group_or_capable(v, gid) {
                mode &= !(c::S_ISGID as c::umode_t);
            }
            bi.bi_mode = mode;
        }
    }

    pub(crate) fn setattr(&self, idmap: MntIdmap<'_>, dentry: Dentry<'_>, attr: &mut c::iattr)
        -> Result<(), BchError>
    {
        self.vfs().assert_locked();
        subvolume::is_ro(self.fs(), self.inum().subvol as u32)?;
        // The kernel crate's struct iattr, codegen's the same
        ret_to_result_void(unsafe {
            c::setattr_prepare(idmap.as_ptr(), dentry.as_ptr(), core::ptr::from_mut(attr).cast())
        })?;

        if attr.ia_valid & ATTR_SIZE != 0 {
            ret_to_result_void(unsafe { bchfs_truncate(idmap.as_ptr(), self.raw(), attr) })
        } else {
            self.setattr_nonsize(idmap, Iattr::of(attr))
        }
    }

    /// The caller has checked it may: setattr_prepare().
    pub(crate) fn setattr_nonsize(&self, idmap: MntIdmap<'_>, attr: &Iattr) -> Result<(), BchError> {
        let lock = self.update_lock();
        let fs = self.fs();
        let v = self.vfs();
        let uid = attr.vfsuid().map(|uid| v.uid_of(idmap.kuid(v, uid)));
        let kgid = attr.vfsgid().map(|gid| idmap.kgid(v, gid));

        let old_qid = self.ei_quota.lock().ei_qid;
        let mut qid = old_qid;
        if let Some(uid) = uid {
            qid.q[c::quota_types::QTYP_USR.0 as usize] = uid;
        }
        if let Some(gid) = kgid {
            qid.q[c::quota_types::QTYP_GRP.0 as usize] = v.gid_of(gid);
        }

        lock.quota_transfer(qid, !0, c::quota_acct_mode::KEY_TYPE_QUOTA_PREALLOC)?;

        let trans = crate::btree_trans!(fs);
        let ret = lockrestart_do(&trans, |t| {
            let mut iter = BtreeIter::uninit();
            let mut bi = inode::__peek(t, &mut iter, self.inum(), BtreeIterFlags::INTENT,
                                       Some(crate::c_function_name!()))?;

            self.fold_times(&mut bi);
            self.setattr_copy(idmap, attr, uid, kgid, &mut bi);

            let acl = if attr.mode().is_some() {
                acl::acl_chmod(t, self.inum(), &bi, bi.bi_mode)?
            } else {
                None
            };

            inode::write(t, &mut iter, &mut bi)?;
            t.commit(None, CommitFlags::NO_ENOSPC)?;

            self.update_after_write(t.trans(), &bi, attr.valid());
            if acl.is_some() {
                self.vfs().set_cached_acl(AclType::Access, acl.as_ref());
            }
            Ok(())
        });

        if ret.is_err() {
            /*
             * The btree commit failed, so the inode on disk still has the old
             * qid: without the transfer undone, the in-memory quota counts
             * diverge from what an inode scan would compute - generic/270
             * catches it. NOCHECK: it's a qid we just came from, which had
             * room.
             */
            let _ = lock.quota_transfer(old_qid, !0, c::quota_acct_mode::KEY_TYPE_QUOTA_NOCHECK);
        }
        ret
    }
}

/// For C's truncate, until it's Rust.
///
/// # Safety
/// @idmap is the mount's, for the call.
#[c_export]
pub unsafe fn bch2_setattr_nonsize(idmap: *mut c::mnt_idmap, inode: &c::bch_inode_info,
                                   attr: &c::iattr) -> c_int {
    ret_to_c(inode.setattr_nonsize(unsafe { MntIdmap::borrow_raw(idmap) }, Iattr::of(attr)))
}
