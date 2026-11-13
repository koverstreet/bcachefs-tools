// SPDX-License-Identifier: GPL-2.0

//! The namespace operations as the VFS makes them: each takes the update
//! locks of the inodes it changes, runs its transaction - fs/namei.rs's - and
//! brings the VFS inodes up to date with what it wrote - and gives the dentry
//! a link or create made its inode. They're a directory's inode ops, DirOps's
//! InodeOps (inode_ops.rs), and InodeInfo's where something else calls them.
//!
//! Lookup writes nothing, unless a dirent and its inode disagree, which it
//! repairs. The inodes it returns come from the inode cache.

use core::ffi::{c_int, c_uint, c_void, CStr};
use core::ptr::{self, null};

use crate::acl::{AclType, PosixAcl};
use crate::init::passes::RecoveryPass;
use crate::btree::bkey::BkeyV;
use crate::btree::iter::{commit_do, lockrestart_do, BtreeIter, BtreeIterFlags, CommitFlags, CommitOpts,
                         TransAttempt};
use crate::c;
use crate::dirent::{self, Dirent};
use crate::errcode::{Errcode, ret_to_c, ret_to_result_void, BchError};
use crate::init::passes;
use crate::inode;
use crate::namei;
use crate::quota::QuotaAcctMode;
use crate::quota;
use crate::snapshots::subvolume;
use crate::str_hash;
use crate::util::kernel::{current_fsgid, current_fsuid};
use crate::util::log::LogMsg;
use crate::util::file::File;
use crate::util::ffi::bytes;
use crate::util::os_str::{cstr_name, OsStr};
use crate::xattr::{self, XattrType};
use cstruct_macros::vtable;
use kernel::sync::aref::ARef;

use super::dentry::Dentry;
use super::idmap::MntIdmap;
use super::inode::{InodeInfo, InodeRef, ATTR_CTIME, ATTR_MTIME, ATTR_SIZE};
use super::inode_ops::InodeOps;
use super::new_inode::NewInode;

const DIR_ENTRIES_CHANGED: u32 = ATTR_MTIME | ATTR_CTIME | ATTR_SIZE;

impl InodeInfo {
    /// A new inode in this directory for @dentry, in the btree and the inode
    /// cache - linked, unless @flags has BCH_CREATE_TMPFILE; a subvolume's
    /// root, a snapshot of @snapshot_src's subvolume, with BCH_CREATE_SUBVOL
    /// and BCH_CREATE_SNAPSHOT. The reference is ours, not yet the dentry's.
    pub(crate) fn create(&self, idmap: MntIdmap<'_>, dentry: Dentry<'_>, mut mode: c::umode_t,
                         rdev: c::dev_t, snapshot_src: c::subvol_inum, flags: u32)
        -> Result<InodeRef, BchError>
    {
        let fs = self.fs();
        let dir = self.vfs();
        let tmpfile = flags & c::BCH_CREATE_TMPFILE != 0;
        let name = (!tmpfile).then_some(dentry);
        let uid = dir.uid_of(idmap.fsuid(dir));
        let gid = dir.gid_of(idmap.fsgid(dir));
        let is_posixacl = dir.sb().flags() as u64 & c::SB_POSIXACL != 0;

        // preallocate acls + vfs inode before btree transaction, so that
        // nothing can fail after the transaction succeeds:
        let (default_acl, acl) = if is_posixacl {
            PosixAcl::create(dir, &mut mode)?
        } else {
            (None, None)
        };

        let mut inode = NewInode::new(fs, c::GFP_NOIO).ok_or(BchError::from(c::ENOMEM))?;
        // so that inserting it, which can't fail, rarely has to allocate
        fs.vfs.inode_cache.make_room();

        let mut inode_u = c::bch_inode_unpacked::default();
        inode::init_early(fs, &mut inode_u);

        let dir_lock = (!tmpfile).then(|| self.update_lock());
        // posix_acl_create() calls get_acl -> btree transaction, don't start
        // ours until after, ei->update_lock must also be taken first:
        let trans = crate::btree_trans!(fs);
        let mut held = None;
        let mut dir_u = c::bch_inode_unpacked::default();
        let mut subvol = c::Subvolume::default();

        // Quotas count only master subvolume inodes; a new subvolume's root is
        // in a new snapshot tree, and is its master unless it's a snapshot:
        let quota = if flags & c::BCH_CREATE_SUBVOL != 0 {
            flags & c::BCH_CREATE_SNAPSHOT == 0
        } else {
            !self.ei_flags.get(c::InodeInfoFlag::EI_INODE_SNAPSHOT)
        };

        let inum = lockrestart_do(&trans, |t| {
            namei::create_trans(t, self.inum(), &mut dir_u, &mut inode_u, &mut subvol,
                                name.map(|d| d.name()), uid, gid, mode, rdev,
                                default_acl.as_ref(), acl.as_ref(), snapshot_src, flags)?;
            let qid = quota::qid(&inode_u);
            if quota {
                quota::acct(fs, qid, quota::Q_INO, 1, QuotaAcctMode::PREALLOC)?;
            }

            let inum = c::subvol_inum {
                subvol: if inode_u.bi_subvol != 0 { inode_u.bi_subvol.into() } else { self.inum().subvol },
                inum:   inode_u.bi_inum,
            };

            let ret = (|| {
                // Before the commit publishes the key. Only a tmpfile needs it:
                // every other create commits with a dirent and nlink 1, which
                // check_inode() doesn't touch.
                if tmpfile {
                    drop(held.take());
                    held = Some(fs.vfs.inode_cache.hold(fs, inum)?);
                }
                init_security(t, self, &mut inode, name.map(|d| d.qstr()), inum, &inode_u, flags)?;
                t.commit(None, CommitFlags::empty())
            })();
            if ret.is_err() && quota {
                let _ = quota::acct(fs, qid, quota::Q_INO, -1, QuotaAcctMode::WARN);
            }
            ret.map(|()| inum)
        })?;

        if !tmpfile {
            self.update_after_write(&trans, &dir_u, DIR_ENTRIES_CHANGED);
        }
        drop(dir_lock);

        inode.init(&trans, inum, &inode_u, &subvol);

        if is_posixacl {
            inode.v.set_cached_acl(AclType::Access, acl.as_ref());
            inode.v.set_cached_acl(AclType::Default, default_acl.as_ref());
        }

        // we must insert the new inode into the inode cache before dropping
        // the transaction and its locks, else we could race with another
        // thread pulling the inode in and modifying it:
        //
        // also, inserting without passing in the transaction is sketchy - if
        // we could ever end up waiting on an inode being freed, we'd risk
        // deadlock.
        //
        // But that shouldn't be possible, since we still have the inode locked
        // that we just created, and we _really_ can't take a transaction
        // restart here.
        Ok(fs.vfs.inode_cache.insert_nofail(inode))
    }

    /// The inode @name in this directory names, through the inode cache - @hash
    /// is this directory's. ENOENT if there's no such name, or it's a subvolume
    /// this directory doesn't list.
    pub fn lookup(&self, hash: &c::bch_hash_info, name: &OsStr) -> Result<InodeRef, BchError> {
        let dir = self.inum();
        // before the locks an insert of what we find would be under
        self.fs().vfs.inode_cache.make_room();
        let trans = crate::btree_trans!(self.fs());

        lockrestart_do(&trans, |t| {
            let mut iter = BtreeIter::uninit();
            let k = dirent::lookup_key(t, &mut iter, dir, hash, name, BtreeIterFlags::empty())?;
            let inum = dirent::lookup_target(t, dir, k)?;

            if let Some(inode) = t.fs().vfs.inode_cache.find(Some(t), inum)? {
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
            dir.fold_times_trans(t)?;
            self.fold_times_trans(t)?;
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
                self.fold_times_trans(t)?;
                inode.fold_times_trans(t)?;
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
}

/// The LSMs' xattrs for a new @inode in @dir.
fn init_security(t: &TransAttempt<'_, '_>, dir: &InodeInfo, inode: &mut NewInode, name: Option<&c::qstr>,
                 inum: c::subvol_inum, inode_u: &c::bch_inode_unpacked, flags: u32) -> Result<(), BchError> {
    // A snapshot create doesn't create an inode: create_trans() looks up the
    // source subvolume's root and reuses it at the new snapshot ID, so the
    // source's security xattr is already visible there through snapshot
    // ancestry. Handing that inode to the LSM would have it compute a fresh
    // label and write it with XATTR_CREATE, which finds the inherited key and
    // fails the whole snapshot with EEXIST_str_hash_set.
    //
    // A snapshot inherits the source's label, which is also the semantics we
    // want - the new root is the same inode, not a new object to be labelled
    // from the calling task. create_trans() skips the ACLs for a snapshot for
    // the same reason; ACLs are xattrs too.
    //
    // The in-core label still gets set: the new subvolume root is
    // instantiated after commit, and the LSM's d_instantiate hook reads it
    // back off the inherited xattr.
    if flags & c::BCH_CREATE_SNAPSHOT != 0 {
        return Ok(());
    }

    // The LSM computes the new label from the task, the dir, and the new
    // inode's mode; the vfs inode isn't initialized until after commit
    // (NewInode::init()), so give it just enough identity here:
    inode.v.set_mode(inode_u.bi_mode);
    inode.v.set_uid(inode_u.bi_uid);
    inode.v.set_gid(inode_u.bi_gid);

    let mut ctx = InitXattrs { t, inum, hash: str_hash::hash_info_init(t.fs(), inode_u)? };
    ret_to_result_void(unsafe {
        c::security_inode_init_security(inode.v.as_ptr(), dir.v.as_ptr(), name.map_or(null(), ptr::from_ref),
                                        Some(initxattrs), (&raw mut ctx).cast())
    })
}

struct InitXattrs<'a, 'b, 'f> {
    t:    &'a TransAttempt<'b, 'f>,
    inum: c::subvol_inum,
    hash: c::bch_hash_info,
}

/// security_inode_init_security()'s: the xattrs the LSMs made, set.
unsafe extern "C" fn initxattrs(_inode: *mut c::inode, xattrs: *const c::xattr, ctx: *mut c_void) -> c_int {
    let ctx = unsafe { &*ctx.cast::<InitXattrs<'_, '_, '_>>() };
    // ended by one with no name
    let mut xattrs = (0..).map(|i| unsafe { &*xattrs.add(i) }).take_while(|x| !x.name.is_null());
    ret_to_c(xattrs.try_for_each(|x| {
        let (name, value) = unsafe { (cstr_name(x.name), bytes(x.value, x.value_len)) };
        xattr::set_key(ctx.t, ctx.inum, &ctx.hash, name, value, XattrType::SECURITY, xattr::XATTR_CREATE)
    }))
}

pub(crate) struct DirOps;

#[vtable]
impl InodeOps for DirOps {
    /// Any error is a negative dentry, as C had it.
    fn lookup(dir: &InodeInfo, dentry: Dentry<'_>, _flags: c_uint) -> *mut c::dentry {
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

    fn create(idmap: MntIdmap<'_>, dir: &InodeInfo, dentry: Dentry<'_>, mode: c::umode_t,
              #[cfg(not(linux_ge = "7.3"))] _excl: bool) -> Result<(), BchError>
    {
        Self::mknod(idmap, dir, dentry, mode | c::S_IFREG as c::umode_t, 0)
    }

    fn link(old: Dentry<'_>, dir: &InodeInfo, dentry: Dentry<'_>) -> Result<(), BchError> {
        let inode = old.inode();
        inode.vfs().assert_locked();

        let fs = dir.fs();
        subvolume::is_ro(fs, dir.inum().subvol as u32)?;
        subvolume::is_ro(fs, inode.inum().subvol as u32)?;
        inode.link(dir, dentry.name())?;
        dentry.instantiate(ARef::from(inode));
        Ok(())
    }

    fn unlink(dir: &InodeInfo, dentry: Dentry<'_>) -> Result<(), BchError> {
        dir.unlink(dentry, false)
    }

    /// The inode made unlinked, its @target written and flushed, then linked
    /// - never a symlink with no target.
    fn symlink(idmap: MntIdmap<'_>, dir: &InodeInfo, dentry: Dentry<'_>, target: &CStr)
        -> Result<(), BchError>
    {
        let inode = dir.create(idmap, dentry, (c::S_IFLNK | c::S_IRWXUGO) as c::umode_t, 0,
                               Default::default(), c::BCH_CREATE_TMPFILE)?;
        let len = target.to_bytes_with_nul().len() as c_int;
        ret_to_result_void(unsafe { c::page_symlink(inode.vfs().as_ptr(), target.as_ptr().cast(), len) })?;
        inode.vfs().write_and_wait()?;
        inode.link(dir, dentry.name())?;
        dentry.instantiate(inode);
        Ok(())
    }

    fn mkdir(idmap: MntIdmap<'_>, dir: &InodeInfo, dentry: Dentry<'_>, mode: c::umode_t)
        -> Result<(), BchError>
    {
        Self::mknod(idmap, dir, dentry, mode | c::S_IFDIR as c::umode_t, 0)
    }

    fn rmdir(dir: &InodeInfo, dentry: Dentry<'_>) -> Result<(), BchError> {
        dir.unlink(dentry, false)
    }

    fn mknod(idmap: MntIdmap<'_>, dir: &InodeInfo, dentry: Dentry<'_>, mode: c::umode_t,
             rdev: c::dev_t) -> Result<(), BchError>
    {
        let inode = dir.create(idmap, dentry, mode, rdev, Default::default(), 0)?;
        dentry.set_casefold_ops(inode.vfs());
        dentry.instantiate(inode);
        Ok(())
    }

    fn rename(_idmap: MntIdmap<'_>, src_dir: &InodeInfo, src_dentry: Dentry<'_>,
              dst_dir: &InodeInfo, dst_dentry: Dentry<'_>, flags: c_uint) -> Result<(), BchError>
    {
        use c::RenameMode::*;

        let fs = src_dir.fs();
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
            return fs.throw(Errcode::BCH_ERR_EINVAL_rename_bad_flags);
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
                for inode in [src_dir, dst_dir, src_inode].into_iter().chain(dst_inode) {
                    inode.fold_times_trans(t)?;
                }
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
                    if !src_dir.ei_flags.get(c::InodeInfoFlag::EI_INODE_SNAPSHOT) {
                        quota::acct(fs, quota::qid(w), c::QuotaCounter::Q_INO.0 as usize, 1,
                                    QuotaAcctMode::PREALLOC)?;
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
        let prj = 1 << c::QuotaType::QTYP_PRJ.0;
        for inode in [Some(src_inode), dst_inode].into_iter().flatten() {
            let _ = locks.get(inode).quota_transfer(quota::qid(&inode.inode()), prj,
                                                    QuotaAcctMode::NOCHECK);
        }
        ret
    }

    fn tmpfile(idmap: MntIdmap<'_>, dir: &InodeInfo, file: &File, mode: c::umode_t)
        -> Result<(), BchError>
    {
        // SAFETY: the file being opened's, referenced by it
        let dentry = unsafe { Dentry::borrow_raw(file.dentry()) };
        let inode = dir.create(idmap, dentry, mode, 0, Default::default(), c::BCH_CREATE_TMPFILE)?;
        unsafe { c::d_mark_tmpfile(file.as_ptr(), inode.vfs().as_ptr()) };
        dentry.instantiate(inode);
        ret_to_result_void(unsafe { c::finish_open_simple(file.as_ptr(), 0) })
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
    passes::run_explicit(fs, &mut msg, RecoveryPass::check_dirents,
                         c::RunRecoveryPassFlags::empty())
}
