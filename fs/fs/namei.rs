// SPDX-License-Identifier: GPL-2.0

//! Names and the inodes they name (fs/namei.c): create, link, unlink and
//! rename - each a dirent and the inodes on both sides of it, updated
//! together in one transaction - walking from an inode back up to its path,
//! and fsck's check that a dirent and the inode it names agree.
//!
//! Every inode keeps a backpointer to a dirent naming it - (bi_dir,
//! bi_dir_offset), cleared when that dirent goes - and the walk up to the
//! root goes through those. A directory has exactly one name; a file may
//! have as many as its link count. A subvolume's root is named in its
//! parent subvolume, by a DT_SUBVOL dirent naming the subvolume, and
//! doesn't count towards its parent directory's link count
//! (is_subdir_for_nlink()).
//!
//! BCH_INODE_has_case_insensitive is set on every directory with a
//! casefolded directory below it, up to the root - for overlayfs, which
//! refuses a casefolded lower layer. Making or moving a directory propagates
//! it up; fsck checks it.
//!
//! The VFS, ioctls, recovery and error reporting are C: each function they
//! call is exported under its C name, at the end of its section.
//!
//! Unsigned arithmetic on link counts and depths wraps, as C's did: a link
//! count that's wrong is fsck's to repair (check_nlinks), and must not be a
//! panic here.

use crate::acl::PosixAcl;
use crate::btree::bkey::{pos, spos, BkeySC};
use crate::btree::iter::{
    is_restart, BtreeIter, BtreeIterFlags, BtreeTrans, CommitFlags, TransAttempt,
    UpdateTriggerFlags,
};
use crate::c;
use crate::c::bch_inode_flags::{BCH_INODE_has_access_acl, BCH_INODE_has_case_insensitive,
                                BCH_INODE_has_default_acl, BCH_INODE_unlinked};
use crate::dirent::{self, d_type_str, Dirent, DirentTarget, Dirents};
use crate::errcode::{bch_errcode, ret_to_c, BchError, Found};
use crate::fs::Fs;
use crate::init::error::{count_fsck_err, id};
use crate::inode;
use crate::opts::reinherit_attrs;
use crate::snapshots::{snapshot, subvolume};
use crate::str_hash;
use crate::util::alloc::{flags::GFP_KERNEL, KVVec};
use crate::util::ffi::Opaque;
use crate::util::kernel::{capable, Capability};
use crate::util::os_str::{qstr_name, OsStr, OsStrExt};
use crate::util::Printbuf;
use crate::{bch_err, fs_inconsistent, fsck_err, fsck_err_on};
use core::ffi::{c_int, c_uint};
use core::fmt;
use core::mem::size_of;
use core::ops::ControlFlow;

const ROOT_SUBVOL_INUM: c::subvol_inum = c::subvol_inum {
    subvol: c::BCACHEFS_ROOT_SUBVOL as u64,
    inum:   c::BCACHEFS_ROOT_INO as u64,
};

fn subvol_inum_eq(l: c::subvol_inum, r: c::subvol_inum) -> bool {
    l.subvol == r.subvol && l.inum == r.inum
}

fn is_root(inode: &c::bch_inode_unpacked) -> bool {
    inode.bi_subvol == c::BCACHEFS_ROOT_SUBVOL && inode.bi_inum == c::BCACHEFS_ROOT_INO as u64
}

/// The directory @inode, which is @inum, is named in: in the parent
/// subvolume, for a subvolume's root.
fn parent_inum(inum: c::subvol_inum, inode: &c::bch_inode_unpacked) -> c::subvol_inum {
    c::subvol_inum {
        subvol: if inode.bi_parent_subvol != 0 { inode.bi_parent_subvol as u64 } else { inum.subvol },
        inum:   inode.bi_dir,
    }
}

fn is_dir(mode: c::umode_t) -> bool {
    mode as u32 & c::S_IFMT == c::S_IFDIR
}

/// The dirent type for an inode of @mode: as mode_to_type().
fn mode_to_type(mode: c::umode_t) -> u8 {
    ((mode as u32 >> 12) & 15) as u8
}

/// Whether @dir's new inodes go below 2^32: as inode_opt_get(c, dir,
/// inodes_32bit). Inode options are stored +1, 0 for unset.
fn inodes_32bit(fs: &Fs, dir: &c::bch_inode_unpacked) -> bool {
    match dir.bi_inodes_32bit {
        0 => fs.opts().inodes_32bit != 0,
        v => v - 1 != 0,
    }
}

// ── Create, link, unlink, rename ─────────────────────────────────────────

/// Create @name in directory @dir, an inode of @mode - filled in from @uid,
/// @gid, @rdev, and its parent - or, with BCH_CREATE_SNAPSHOT, a snapshot of
/// subvolume @snapshot_src, its root found if @snapshot_src.inum is 0.
/// BCH_CREATE_SUBVOL makes the new directory a subvolume's root,
/// BCH_CREATE_SNAPSHOT_RO the snapshot read-only, and BCH_CREATE_TMPFILE
/// gives the inode no name - @name is None then, and only then. As
/// bch2_create_trans().
///
/// @dir_u and @new_inode are read and returned; @new_subvol is @dir's
/// subvolume, or the new one if one's created. The new inode gets @acl and
/// @default_acl - none, in userspace, which has no struct posix_acl.
#[allow(clippy::too_many_arguments)]
pub fn create_trans(
    t:                &TransAttempt<'_, '_>,
    dir:              c::subvol_inum,
    dir_u:            &mut c::bch_inode_unpacked,
    new_inode:        &mut c::bch_inode_unpacked,
    new_subvol:       &mut c::bch_subvolume,
    name:             Option<&OsStr>,
    uid:              c::uid_t,
    gid:              c::gid_t,
    mode:             c::umode_t,
    rdev:             c::dev_t,
    default_acl:      Option<&PosixAcl>,
    acl:              Option<&PosixAcl>,
    mut snapshot_src: c::subvol_inum,
    mut flags:        u32,
) -> Result<(), BchError> {
    let fs = t.fs();
    let mut dir_iter = BtreeIter::uninit();
    let mut inode_iter = BtreeIter::uninit();
    let now = fs.current_time();
    let mut dir_type = mode_to_type(mode);

    *new_subvol = subvolume::get(t, dir.subvol as u32, true)?;
    if new_subvol.ro() ||
       new_subvol.state() == Some(c::bch_subvolume_state::SUBVOLUME_STATE_unlinked) {
        return Err(BchError::from(c::EROFS));
    }

    let mut dir_snapshot = new_subvol.snapshot();
    let mut child_snapshot = dir_snapshot;

    *dir_u = inode::peek_snapshot(t, &mut dir_iter, dir, dir_snapshot, BtreeIterFlags::INTENT)?;

    if flags & c::BCH_CREATE_SNAPSHOT == 0 {
        // Normal create path - allocate a new inode:
        inode::init_late(fs, new_inode, now, uid, gid, mode, rdev, Some(dir_u));

        if flags & c::BCH_CREATE_TMPFILE != 0 {
            new_inode.set_flag(BCH_INODE_unlinked, true);
        }

        if acl.is_some() {
            new_inode.set_flag(BCH_INODE_has_access_acl, true);
        }
        if default_acl.is_some() {
            new_inode.set_flag(BCH_INODE_has_default_acl, true);
        }

        inode::create(t, &mut inode_iter, new_inode, dir_snapshot, inodes_32bit(fs, dir_u))?;

        snapshot_src = c::subvol_inum::default();
    } else {
        // Creating a snapshot - we're not allocating a new inode, but we do
        // have to look up the root inode of the subvolume we're snapshotting
        // and update it (in the new snapshot):
        if snapshot_src.inum == 0 {
            // Inode wasn't specified, just snapshot:
            let s = subvolume::get(t, snapshot_src.subvol as u32, true)?;
            snapshot_src.inum = u64::from_le(s.inode);
        }

        *new_inode = inode::peek(t, &mut inode_iter, snapshot_src, BtreeIterFlags::INTENT)?;

        // Not a subvolume root?
        if new_inode.bi_subvol as u64 != snapshot_src.subvol {
            return Err(fs.err(bch_errcode::BCH_ERR_EINVAL_snapshot_not_subvol_root));
        }

        // If we're not root, we have to own the subvolume being snapshotted
        // - in that order: capable() records using the capability.
        if uid != 0 && !capable(Capability::Fowner) && new_inode.bi_uid != uid {
            return Err(fs.err(bch_errcode::BCH_ERR_EPERM_non_admin_or_owner));
        }

        flags |= c::BCH_CREATE_SUBVOL;
    }

    let mut dir_target = DirentTarget::Inode(new_inode.bi_inum);

    if flags & c::BCH_CREATE_SUBVOL != 0 {
        let created = subvolume::create(t, new_inode.bi_inum, dir.subvol as u32,
                                        snapshot_src.subvol as u32,
                                        flags & c::BCH_CREATE_SNAPSHOT_RO != 0)?;
        child_snapshot = created.snapshot;
        *new_subvol    = created.v;

        new_inode.bi_parent_subvol = dir.subvol as u32;
        new_inode.bi_subvol        = created.subvol;
        dir_target                 = DirentTarget::Subvol { child: created.subvol,
                                                            parent: dir.subvol as u32 };
        dir_type                   = c::DT_SUBVOL as u8;

        dir_snapshot = subvolume::get_snapshot(t, dir.subvol as u32)?;

        dir_iter.set_snapshot(dir_snapshot);
        dir_iter.traverse(t)?;
    }

    if flags & c::BCH_CREATE_SNAPSHOT == 0 {
        #[cfg(kernel)]
        {
            use crate::acl::{set_acl_trans, ACL_TYPE_ACCESS, ACL_TYPE_DEFAULT};

            // In the new subvolume, if one was made:
            let new_inum = c::subvol_inum {
                subvol: if flags & c::BCH_CREATE_SUBVOL != 0 {
                    new_inode.bi_subvol as u64
                } else {
                    dir.subvol
                },
                inum:   new_inode.bi_inum,
            };

            if default_acl.is_some() {
                set_acl_trans(t, new_inum, new_inode, default_acl, ACL_TYPE_DEFAULT)?;
            }
            if acl.is_some() {
                set_acl_trans(t, new_inum, new_inode, acl, ACL_TYPE_ACCESS)?;
            }
        }
    }

    if flags & c::BCH_CREATE_TMPFILE == 0 {
        let name = name.expect("create_trans: no name, and not BCH_CREATE_TMPFILE");

        dir_u.bi_nlink = dir_u.bi_nlink.wrapping_add(new_inode.is_subdir_for_nlink() as u32);
        dir_u.bi_mtime = now;
        dir_u.bi_ctime = now;

        let mut dir_offset = 0;
        dirent::create_snapshot(t, dir.subvol as u32, dir_snapshot, dir_u, dir_type, name,
                                dir_target, &mut dir_offset,
                                BtreeIterFlags::STR_HASH_MUST_CREATE,
                                UpdateTriggerFlags::empty())?;
        inode::write(t, &mut dir_iter, dir_u)?;

        new_inode.bi_dir        = dir_u.bi_inum;
        new_inode.bi_dir_offset = dir_offset;
    }

    if is_dir(mode) {
        let subvol = if new_inode.bi_subvol != 0 { new_inode.bi_subvol as u64 } else { dir.subvol };
        let inum = c::subvol_inum { subvol, inum: new_inode.bi_inum };
        maybe_propagate_has_case_insensitive(t, inum, new_inode)?;
    }

    if is_dir(mode) && new_inode.bi_subvol == 0 {
        new_inode.bi_depth = dir_u.bi_depth.wrapping_add(1);
    }

    inode_iter.clear_flags(BtreeIterFlags::ALL_SNAPSHOTS);
    inode_iter.set_snapshot(child_snapshot);

    inode_iter.traverse(t)?;
    inode::write(t, &mut inode_iter, new_inode)
}

/// For C's VFS and recovery: bch2_create_trans().
///
/// # Safety
/// @name and the ACLs valid for the call; in userspace the ACLs are NULL.
/// @name is NULL for BCH_CREATE_TMPFILE.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn bch2_create_trans(
    trans:        &Opaque<c::btree_trans>,
    dir:          c::subvol_inum,
    dir_u:        &mut c::bch_inode_unpacked,
    new_inode:    &mut c::bch_inode_unpacked,
    new_subvol:   &mut c::bch_subvolume,
    name:         Option<&c::qstr>,
    uid:          c::uid_t,
    gid:          c::gid_t,
    mode:         c::umode_t,
    rdev:         c::dev_t,
    default_acl:  *mut c::posix_acl,
    acl:          *mut c::posix_acl,
    snapshot_src: c::subvol_inum,
    flags:        c_uint,
) -> c_int {
    let trans = BtreeTrans::from_c(trans);

    #[cfg(kernel)]
    let acls = unsafe { (PosixAcl::borrow_raw(default_acl), PosixAcl::borrow_raw(acl)) };
    #[cfg(kernel)]
    let (default_acl, acl) = (acls.0.as_deref(), acls.1.as_deref());
    #[cfg(not(kernel))]
    let (default_acl, acl): (Option<&PosixAcl>, Option<&PosixAcl>) = {
        assert!(default_acl.is_null() && acl.is_null(), "a posix_acl in userspace");
        (None, None)
    };

    ret_to_c(create_trans(&trans.attempt_in_progress(), dir, dir_u, new_inode, new_subvol,
                          name.map(|name| unsafe { qstr_name(name) }),
                          uid, gid, mode, rdev, default_acl, acl, snapshot_src, flags))
}

/// Link inode @inum into directory @dir as @name - not across subvolumes,
/// or into a directory whose options it would have to take on: as
/// bch2_link_trans(). @dir_u and @inode_u are read and returned.
pub fn link_trans(
    t:       &TransAttempt<'_, '_>,
    dir:     c::subvol_inum,
    dir_u:   &mut c::bch_inode_unpacked,
    inum:    c::subvol_inum,
    inode_u: &mut c::bch_inode_unpacked,
    name:    &OsStr,
) -> Result<(), BchError> {
    let fs = t.fs();
    let mut dir_iter = BtreeIter::uninit();
    let mut inode_iter = BtreeIter::uninit();
    let now = fs.current_time();
    let mut dir_offset = 0;

    if dir.subvol != inum.subvol {
        return Err(BchError::from(c::EXDEV));
    }

    *inode_u = inode::peek(t, &mut inode_iter, inum, BtreeIterFlags::INTENT)?;

    inode_u.bi_ctime = now;
    inode::nlink_inc(inode_u)?;

    *dir_u = inode::peek(t, &mut dir_iter, dir, BtreeIterFlags::INTENT)?;

    if reinherit_attrs(inode_u, dir_u) {
        return Err(BchError::from(c::EXDEV));
    }

    dir_u.bi_mtime = now;
    dir_u.bi_ctime = now;

    dirent::create(t, dir, dir_u, mode_to_type(inode_u.bi_mode), name, inum.inum,
                   &mut dir_offset, BtreeIterFlags::STR_HASH_MUST_CREATE,
                   UpdateTriggerFlags::empty())?;

    inode_u.bi_dir        = dir.inum;
    inode_u.bi_dir_offset = dir_offset;

    inode::write(t, &mut dir_iter, dir_u)?;
    inode::write(t, &mut inode_iter, inode_u)
}

/// For C's VFS: bch2_link_trans().
///
/// # Safety
/// @name valid for the call.
#[no_mangle]
pub unsafe extern "C" fn bch2_link_trans(
    trans:   &Opaque<c::btree_trans>,
    dir:     c::subvol_inum,
    dir_u:   &mut c::bch_inode_unpacked,
    inum:    c::subvol_inum,
    inode_u: &mut c::bch_inode_unpacked,
    name:    &c::qstr,
) -> c_int {
    ret_to_c(link_trans(&BtreeTrans::from_c(trans).attempt_in_progress(), dir, dir_u, inum,
                        inode_u, unsafe { qstr_name(name) }))
}

/// The VFS asked to unlink @wanted as @name, and @name names @got: the
/// dcache and the btree disagree, and neither can be trusted - go emergency
/// read-only, and say so with @wanted's path.
fn report_bad_unlink(
    t:      &TransAttempt<'_, '_>,
    wanted: c::subvol_inum,
    got:    c::subvol_inum,
    name:   &OsStr,
) -> Result<(), BchError> {
    let fs = t.fs();
    let mut msg = Printbuf::new();

    writeln!(msg, "vfs did bad unlink: wanted inum {}:{}, got {}:{}",
             wanted.subvol, wanted.inum, got.subvol, got.inum);
    writeln!(msg, "vfs d_name {:?}", name);
    write!(msg, "path ");

    let ret = inum_to_path(t, wanted, &mut msg);
    let print = match ret {
        Ok(()) => {
            fs.emergency_read_only(&mut msg);
            count_fsck_err(fs, id::vfs_unlink_got_wrong_inum, &mut msg)
        }
        Err(_) => true,
    };

    if print {
        bch_err!(fs, "{}", msg);
    }
    ret
}

/// Unlink @name from directory @dir - inode @inode, if the caller knows
/// which it should be; with @deleting_subvol, a subvolume's root, deleting
/// the subvolume: as bch2_unlink_trans(). @dir_u and @inode_u are read and
/// returned.
#[allow(clippy::too_many_arguments)]
pub fn unlink_trans(
    t:               &TransAttempt<'_, '_>,
    dir:             c::subvol_inum,
    dir_u:           &mut c::bch_inode_unpacked,
    inode:           c::subvol_inum,
    inode_u:         &mut c::bch_inode_unpacked,
    name:            &OsStr,
    deleting_subvol: bool,
) -> Result<(), BchError> {
    let fs = t.fs();
    let mut dir_iter = BtreeIter::uninit();
    let mut dirent_iter = BtreeIter::uninit();
    let mut inode_iter = BtreeIter::uninit();
    let now = fs.current_time();

    let snapshot = if !deleting_subvol {
        subvolume::is_ro_trans(t, dir.subvol as u32)?
    } else {
        subvolume::get_snapshot(t, dir.subvol as u32)?
    };

    *dir_u = inode::peek_snapshot(t, &mut dir_iter, dir, snapshot, BtreeIterFlags::INTENT)?;

    let dir_hash = str_hash::hash_info_init(fs, dir_u)?;

    let inum = dirent::lookup_snapshot(t, &mut dirent_iter, dir, snapshot, &dir_hash, name,
                                       BtreeIterFlags::INTENT)?;

    if (inode.subvol != 0 || inode.inum != 0) && !subvol_inum_eq(inode, inum) {
        report_bad_unlink(t, inode, inum, name)?;
    }

    *inode_u = inode::peek(t, &mut inode_iter, inum, BtreeIterFlags::INTENT)?;

    if !deleting_subvol && inode_u.is_dir() {
        dirent::empty_dir_trans(t, inum)?;
    }

    if deleting_subvol && inode_u.bi_subvol == 0 {
        return Err(fs.err(bch_errcode::BCH_ERR_ENOENT_not_subvol));
    }

    // Recursive subvolume destroy not allowed (yet?)
    if inode_u.bi_subvol != 0 {
        subvolume::require_no_children(t, inode_u.bi_subvol)?;
    }

    if deleting_subvol || inode_u.bi_subvol != 0 {
        subvolume::unlink(t, inode_u.bi_subvol)?;

        // No dirent will ever point at this inode again - deletion belongs
        // to the subvolume path, though: the inode reaper keys off
        // bch2_inode_is_subvolume_root() to leave it alone.
        inode_u.set_flag(BCH_INODE_unlinked, true);

        // If we're deleting a subvolume, we need to really delete the
        // dirent, not just emit a whiteout in the current snapshot:
        let k = dirent_iter.peek_slot(t)?.expect("a slot always has a key");
        let k_snapshot = k.k.p.snapshot;

        dirent_iter.set_snapshot(k_snapshot);
        dirent_iter.traverse(t)?;
    } else {
        inode::nlink_dec(t, inode_u);
    }

    let dirent_pos = dirent_iter.pos();
    if inode_u.bi_dir        == dirent_pos.inode &&
       inode_u.bi_dir_offset == dirent_pos.offset {
        inode_u.clear_backpointer();
    }

    dir_u.bi_mtime   = now;
    dir_u.bi_ctime   = now;
    inode_u.bi_ctime = now;
    dir_u.bi_nlink   = dir_u.bi_nlink.wrapping_sub(inode_u.is_subdir_for_nlink() as u32);

    str_hash::delete_at::<Dirents>(t, &dir_hash, &mut dirent_iter,
                                   UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)?;
    inode::write(t, &mut dir_iter, dir_u)?;
    inode::write(t, &mut inode_iter, inode_u)
}

/// For C's VFS: bch2_unlink_trans().
///
/// # Safety
/// @name valid for the call.
#[no_mangle]
pub unsafe extern "C" fn bch2_unlink_trans(
    trans:           &Opaque<c::btree_trans>,
    dir:             c::subvol_inum,
    dir_u:           &mut c::bch_inode_unpacked,
    inode:           c::subvol_inum,
    inode_u:         &mut c::bch_inode_unpacked,
    name:            &c::qstr,
    deleting_subvol: bool,
) -> c_int {
    ret_to_c(unlink_trans(&BtreeTrans::from_c(trans).attempt_in_progress(), dir, dir_u, inode,
                          inode_u, unsafe { qstr_name(name) }, deleting_subvol))
}

/// Subvolume @subvol's path in the filesystem now starts in subvolume
/// @new_parent.
fn subvol_update_parent(t: &TransAttempt<'_, '_>, subvol: u32, new_parent: u32)
    -> Result<(), BchError>
{
    let mut s = t.bkey_get_mut(c::btree_id::subvolumes, pos(0, subvol as u64),
                               BtreeIterFlags::CACHED, UpdateTriggerFlags::empty(),
                               c::bch_bkey_type::KEY_TYPE_subvolume,
                               size_of::<c::bkey_i_subvolume>())?;
    s.k_i_mut().as_mut_subvolume().expect("a subvolume").fs_path_parent = new_parent.to_le();
    Ok(())
}

/// Rename @src_name in directory @src_dir to @dst_name in @dst_dir - for
/// BCH_RENAME_OVERWRITE replacing what's there, for BCH_RENAME_EXCHANGE
/// swapping the two: as bch2_rename_trans(). The directories and inodes are
/// read and returned; @dst_dir_u isn't touched if it's the same directory.
///
/// An inode moved to another directory takes on its options, and if that
/// changes what its data should be, @src_opt_change and @dst_opt_change say
/// so: rename_opt_changes_finish(), after the commit, acts on them.
#[allow(clippy::too_many_arguments)]
pub fn rename_trans(
    t:              &TransAttempt<'_, '_>,
    src_dir:        c::subvol_inum,
    src_dir_u:      &mut c::bch_inode_unpacked,
    dst_dir:        c::subvol_inum,
    dst_dir_u:      &mut c::bch_inode_unpacked,
    src_inode_u:    &mut c::bch_inode_unpacked,
    dst_inode_u:    &mut c::bch_inode_unpacked,
    src_name:       &OsStr,
    dst_name:       &OsStr,
    mode:           c::bch_rename_mode,
    src_opt_change: &mut c::inode_opt_change,
    dst_opt_change: &mut c::inode_opt_change,
) -> Result<(), BchError> {
    use c::bch_rename_mode::*;

    let fs = t.fs();
    let mut src_dir_iter = BtreeIter::uninit();
    let mut dst_dir_iter = BtreeIter::uninit();
    let mut src_inode_iter = BtreeIter::uninit();
    let mut dst_inode_iter = BtreeIter::uninit();
    let now = fs.current_time();
    let same_dir = subvol_inum_eq(dst_dir, src_dir);

    // The destination directory - the source directory, if they're the
    // same: C aliased the two pointers.
    macro_rules! dst_dir_u {
        () => { if same_dir { &mut *src_dir_u } else { &mut *dst_dir_u } };
    }

    *src_opt_change = c::inode_opt_change::new();
    *dst_opt_change = c::inode_opt_change::new();

    *src_dir_u = inode::peek(t, &mut src_dir_iter, src_dir, BtreeIterFlags::INTENT)?;

    let src_hash = str_hash::hash_info_init(fs, src_dir_u)?;

    let dst_dir_hash;
    let dst_hash = if !same_dir {
        *dst_dir_u = inode::peek(t, &mut dst_dir_iter, dst_dir, BtreeIterFlags::INTENT)?;

        dst_dir_hash = str_hash::hash_info_init(fs, dst_dir_u)?;
        &dst_dir_hash
    } else {
        &src_hash
    };

    let r = dirent::rename(t, src_dir, &src_hash, dst_dir, dst_hash, src_name, dst_name, mode)?;
    let (src_inum, dst_inum) = (r.src_inum, r.dst_inum);

    *src_inode_u = inode::peek(t, &mut src_inode_iter, src_inum, BtreeIterFlags::INTENT)?;
    let src_old_r = inode::reconcile_opts_get(fs, src_inode_u);

    let mut dst_old_r = c::bch_extent_reconcile::default();
    if dst_inum.inum != 0 {
        *dst_inode_u = inode::peek(t, &mut dst_inode_iter, dst_inum, BtreeIterFlags::INTENT)?;
        dst_old_r = inode::reconcile_opts_get(fs, dst_inode_u);
    }

    if src_inode_u.bi_subvol != 0 &&
       dst_dir.subvol != src_inode_u.bi_parent_subvol as u64 {
        subvol_update_parent(t, src_inode_u.bi_subvol, dst_dir.subvol as u32)?;
    }

    if mode == BCH_RENAME_EXCHANGE &&
       dst_inode_u.bi_subvol != 0 &&
       src_dir.subvol != dst_inode_u.bi_parent_subvol as u64 {
        subvol_update_parent(t, dst_inode_u.bi_subvol, src_dir.subvol as u32)?;
    }

    // Can't move across subvolumes, unless it's a subvolume root:
    if src_dir.subvol != dst_dir.subvol &&
       (src_inode_u.bi_subvol == 0 ||
        (dst_inum.inum != 0 && dst_inode_u.bi_subvol == 0)) {
        return Err(BchError::from(c::EXDEV));
    }

    if src_inode_u.bi_parent_subvol != 0 {
        src_inode_u.bi_parent_subvol = dst_dir.subvol as u32;
    }

    if mode == BCH_RENAME_EXCHANGE && dst_inode_u.bi_parent_subvol != 0 {
        dst_inode_u.bi_parent_subvol = src_dir.subvol as u32;
    }

    src_inode_u.bi_dir        = dst_dir_u!().bi_inum;
    src_inode_u.bi_dir_offset = r.dst_offset;

    if mode == BCH_RENAME_EXCHANGE {
        dst_inode_u.bi_dir        = src_dir_u.bi_inum;
        dst_inode_u.bi_dir_offset = r.src_offset;
    }

    if mode == BCH_RENAME_OVERWRITE &&
       dst_inode_u.bi_dir        == dst_dir_u!().bi_inum &&
       dst_inode_u.bi_dir_offset == r.src_offset {
        dst_inode_u.clear_backpointer();
    }

    if mode == BCH_RENAME_OVERWRITE {
        if src_inode_u.is_dir() != dst_inode_u.is_dir() {
            return Err(BchError::from(c::ENOTDIR));
        }

        if dst_inode_u.is_dir() {
            dirent::empty_dir_trans(t, dst_inum)?;
        }
    }

    if !same_dir {
        if reinherit_attrs(src_inode_u, dst_dir_u!()) && src_inode_u.is_dir() {
            return Err(BchError::from(c::EXDEV));
        }

        if mode == BCH_RENAME_EXCHANGE &&
           reinherit_attrs(dst_inode_u, src_dir_u) && dst_inode_u.is_dir() {
            return Err(BchError::from(c::EXDEV));
        }

        // Reinherited options have to reach existing data too
        src_opt_change.record(t, &src_old_r, src_inode_u, src_inode_iter.snapshot())?;
        if mode == BCH_RENAME_EXCHANGE {
            dst_opt_change.record(t, &dst_old_r, dst_inode_u, dst_inode_iter.snapshot())?;
        }

        if src_inode_u.is_subdir_for_nlink() {
            src_dir_u.bi_nlink = src_dir_u.bi_nlink.wrapping_sub(1);
            dst_dir_u!().bi_nlink = dst_dir_u!().bi_nlink.wrapping_add(1);
        }

        if src_inode_u.is_dir() && src_inode_u.bi_subvol == 0 {
            src_inode_u.bi_depth = dst_dir_u!().bi_depth.wrapping_add(1);
        }

        if mode == BCH_RENAME_EXCHANGE &&
           dst_inode_u.is_dir() && dst_inode_u.bi_subvol == 0 {
            dst_inode_u.bi_depth = src_dir_u.bi_depth.wrapping_add(1);
        }
    }

    if dst_inum.inum != 0 && dst_inode_u.is_subdir_for_nlink() {
        dst_dir_u!().bi_nlink = dst_dir_u!().bi_nlink.wrapping_sub(1);
        src_dir_u.bi_nlink = src_dir_u.bi_nlink.wrapping_add((mode == BCH_RENAME_EXCHANGE) as u32);
    }

    if mode == BCH_RENAME_OVERWRITE {
        // Overwriting a subvolume root deletes the subvolume, same as unlink
        // (the victim was empty or a bare file, per the checks above): hand
        // it to the subvolume deletion path, don't treat it as an ordinary
        // inode losing its last link - see unlink_trans():
        if dst_inode_u.bi_subvol != 0 {
            subvolume::require_no_children(t, dst_inode_u.bi_subvol)?;
            subvolume::unlink(t, dst_inode_u.bi_subvol)?;
            dst_inode_u.set_flag(BCH_INODE_unlinked, true);
        } else {
            inode::nlink_dec(t, dst_inode_u);
        }
    }

    src_dir_u.bi_mtime = now;
    src_dir_u.bi_ctime = now;

    if src_dir.inum != dst_dir.inum {
        dst_dir_u!().bi_mtime = now;
        dst_dir_u!().bi_ctime = now;
    }

    src_inode_u.bi_ctime = now;

    if dst_inum.inum != 0 {
        dst_inode_u.bi_ctime = now;
    }

    inode::write(t, &mut src_dir_iter, src_dir_u)?;
    if !same_dir {
        inode::write(t, &mut dst_dir_iter, dst_dir_u)?;
    }

    inode::write(t, &mut src_inode_iter, src_inode_u)?;
    if dst_inum.inum != 0 {
        inode::write(t, &mut dst_inode_iter, dst_inode_u)?;
    }

    if !same_dir {
        maybe_propagate_has_case_insensitive(t, src_inum, src_inode_u)?;
        if mode == BCH_RENAME_EXCHANGE {
            maybe_propagate_has_case_insensitive(t, dst_inum, dst_inode_u)?;
        }
    }

    Ok(())
}

/// For C's VFS: bch2_rename_trans().
///
/// # Safety
/// The names valid for the call; the inodes four different ones, as their
/// &muts say.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn bch2_rename_trans(
    trans:          &Opaque<c::btree_trans>,
    src_dir:        c::subvol_inum,
    src_dir_u:      &mut c::bch_inode_unpacked,
    dst_dir:        c::subvol_inum,
    dst_dir_u:      &mut c::bch_inode_unpacked,
    src_inode_u:    &mut c::bch_inode_unpacked,
    dst_inode_u:    &mut c::bch_inode_unpacked,
    src_name:       &c::qstr,
    dst_name:       &c::qstr,
    mode:           c::bch_rename_mode,
    src_opt_change: &mut c::inode_opt_change,
    dst_opt_change: &mut c::inode_opt_change,
) -> c_int {
    ret_to_c(rename_trans(&BtreeTrans::from_c(trans).attempt_in_progress(), src_dir, src_dir_u,
                          dst_dir, dst_dir_u, src_inode_u, dst_inode_u,
                          unsafe { qstr_name(src_name) }, unsafe { qstr_name(dst_name) },
                          mode, src_opt_change, dst_opt_change))
}

/// After rename_trans() has committed: what it left in @src and @dst to do.
pub fn rename_opt_changes_finish(
    fs:  &Fs,
    src: &mut c::inode_opt_change,
    dst: &mut c::inode_opt_change,
) -> Result<(), BchError> {
    let trans = crate::btree_trans!(fs);
    src.finish(&trans)?;
    dst.finish(&trans)
}

// ── From an inode to its path ────────────────────────────────────────────

/// The dirent naming @inode, looked up through @iter: as
/// bch2_inode_get_dirent(). @snapshot is where to look, and on return where
/// the lookup looked - a subvolume root is named in its parent subvolume.
pub fn inode_get_dirent<'i, 't>(
    t:        &'i TransAttempt<'_, 't>,
    iter:     &'i mut BtreeIter<'t>,
    inode:    &c::bch_inode_unpacked,
    snapshot: &mut u32,
) -> Result<BkeySC<'i>, BchError> {
    if inode.bi_parent_subvol != 0 {
        *snapshot = subvolume::get_snapshot(t, inode.bi_parent_subvol)?;
    }

    // If we're running after an interrupted snapshot deletion, the dirent
    // may have been moved to a child snapshot (when cleaning up redundant
    // interior node snapshots) but not the inode - do the lookup in the
    // child snapshot we'll be moving to:
    *snapshot = snapshot::redundant_interior(t.fs(), *snapshot).unwrap_or(*snapshot);

    *iter = BtreeIter::new(t, c::btree_id::dirents,
                           spos(inode.bi_dir, inode.bi_dir_offset, *snapshot),
                           BtreeIterFlags::empty());
    iter.peek_slot_typed(t, c::bch_bkey_type::KEY_TYPE_dirent)
}

/// A path, as inum_to_path walks it: up from the inode, a name per step;
/// printed from the root down. @note is why the walk stopped short of the
/// root, if it did.
struct PathUp {
    names: KVVec<u8>,
    ends:  KVVec<usize>,
    note:  Printbuf,
}

impl PathUp {
    fn new() -> Self {
        PathUp { names: KVVec::new(), ends: KVVec::new(), note: Printbuf::new() }
    }

    fn push(&mut self, name: &OsStr) -> Result<(), BchError> {
        self.names.extend_from_slice(name.as_bytes(), GFP_KERNEL)?;
        self.ends.push(self.names.len(), GFP_KERNEL)?;
        Ok(())
    }

    fn print(&self, out: &mut Printbuf) {
        write!(out, "{}", self.note);

        for i in (0..self.ends.len()).rev() {
            let start = if i > 0 { self.ends[i - 1] } else { 0 };
            write!(out, "/");
            out.write_bytes(&self.names[start..self.ends[i]]);
        }
    }

    fn is_empty(&self) -> bool {
        self.ends.is_empty() && self.note.as_bytes().is_empty()
    }
}

/// The snapshot of the first version of inode @inum.
fn first_inode_snapshot(t: &TransAttempt<'_, '_>, inum: u64) -> Result<u32, BchError> {
    let mut iter = BtreeIter::new(t, c::btree_id::inodes, pos(0, inum),
                                  BtreeIterFlags::ALL_SNAPSHOTS);

    iter.for_each_max_norestart(t, spos(0, inum, u32::MAX), |_, k| Ok(
        if inode::bkey_is_inode(k.k) {
            ControlFlow::Break(Some(k.k.p.snapshot))
        } else {
            ControlFlow::Continue(())
        }))?
        .ok_or_else(|| t.fs().err(bch_errcode::BCH_ERR_ENOENT_snapshot))
}

/// Walk up from inode @inum - in subvolume @subvol, or in @snapshot -
/// through the dirents naming each inode, to the root, or to subvolume
/// @stop_subvol's root, collecting the names into @path. A loop, or a walk
/// that can't go on, ends it with a note in @path - an error, with
/// @fail_on_err. Allocation failure is an error either way.
fn inum_to_path_up(
    t:           &TransAttempt<'_, '_>,
    mut subvol:  u32,
    mut inum:    u64,
    mut snapshot: u32,
    stop_subvol: u32,
    fail_on_err: bool,
    path:        &mut PathUp,
) -> Result<(), BchError> {
    let fs = t.fs();
    let mut seen: KVVec<(u32, u64)> = KVVec::new();

    let mut ret = Ok(());

    if snapshot == 0 {
        ret = if subvol != 0 {
            subvolume::get_snapshot(t, subvol)
        } else {
            first_inode_snapshot(t, inum)
        }.map(|s| snapshot = s);
    }

    while ret.is_ok() {
        let n = (if subvol != 0 { subvol } else { snapshot }, inum);

        if seen.contains(&n) {
            write!(path.note, "(loop at {}:{})", inum, snapshot);
            break;
        }

        seen.push(n, GFP_KERNEL)?;

        let inode = match inode::find_by_inum_snapshot(t, inum, snapshot, BtreeIterFlags::empty()) {
            Ok(inode) => inode,
            Err(e)    => { ret = Err(e); break; }
        };

        if if stop_subvol != 0 { inode.bi_subvol == stop_subvol } else { is_root(&inode) } {
            break;
        }

        if inode.bi_dir == 0 && inode.bi_dir_offset == 0 {
            ret = Err(fs.err(bch_errcode::BCH_ERR_ENOENT_inode_no_backpointer));
            break;
        }

        let mut d_iter = BtreeIter::uninit();
        match inode_get_dirent(t, &mut d_iter, &inode, &mut snapshot) {
            Ok(d)  => path.push(Dirent::new(d).expect("a dirent").name())?,
            Err(e) => { ret = Err(e); break; }
        }

        // Track the subvol as we cross boundaries: the loop-detection key
        // above is (subvol ?: snapshot, inum), and inode numbers repeat
        // across subvolumes (a snapshot shares its source's root inum), so
        // without this the key collides and a valid path reads as a loop.
        if inode.bi_parent_subvol != 0 {
            subvol = inode.bi_parent_subvol;
        }

        inum = inode.bi_dir;
    }

    match ret {
        Err(e) if !is_restart(&e) && !fail_on_err => {
            write!(path.note, "({}: disconnected at {}.{})", e, inum, snapshot);
            Ok(())
        }
        ret => ret,
    }
}

/// Print @inum's path, as inum_to_path_up() finds it, to @out - nothing on a
/// restart, which is retried, but what was found on any other error.
#[allow(clippy::too_many_arguments)]
fn __inum_to_path(
    t:           &TransAttempt<'_, '_>,
    subvol:      u32,
    inum:        u64,
    snapshot:    u32,
    stop_subvol: u32,
    fail_on_err: bool,
    out:         &mut Printbuf,
) -> Result<(), BchError> {
    let mut path = PathUp::new();
    let ret = inum_to_path_up(t, subvol, inum, snapshot, stop_subvol, fail_on_err, &mut path);

    match &ret {
        Err(e) if is_restart(e) => {}
        Ok(()) if path.is_empty() => write!(out, "/"),
        _ => path.print(out),
    }
    ret
}

/// Print @inum's path - its names through its dirents, from the root - to
/// @out: as bch2_inum_to_path(). In the caller's transaction attempt.
pub fn inum_to_path(trans: &BtreeTrans<'_>, inum: c::subvol_inum, out: &mut Printbuf)
    -> Result<(), BchError>
{
    __inum_to_path(&trans.attempt_in_progress(), inum.subvol as u32, inum.inum, 0, 0, false, out)
}

/// For C's error reporting, ioctls and VFS: bch2_inum_to_path().
///
#[no_mangle]
pub extern "C" fn bch2_inum_to_path(
    trans: &Opaque<c::btree_trans>,
    inum:  c::subvol_inum,
    out:   &mut Printbuf,
) -> c_int {
    ret_to_c(inum_to_path(&BtreeTrans::from_c(trans), inum, out))
}

/// For C's ioctls: bch2_inum_to_path_in_subvol() - @inum's path from
/// subvolume @stop_subvol's root, with INUM_TO_PATH_FAIL_ON_ERR an error if
/// the walk can't get there.
#[no_mangle]
pub extern "C" fn bch2_inum_to_path_in_subvol(
    trans:       &Opaque<c::btree_trans>,
    inum:        c::subvol_inum,
    stop_subvol: u32,
    flags:       c_uint,
    out:         &mut Printbuf,
) -> c_int {
    const INUM_TO_PATH_FAIL_ON_ERR: c_uint = 1 << 0;

    ret_to_c(__inum_to_path(&BtreeTrans::from_c(trans).attempt_in_progress(),
                            inum.subvol as u32, inum.inum, 0,
                            stop_subvol, flags & INUM_TO_PATH_FAIL_ON_ERR != 0, out))
}

/// Print the path of inode @inum as seen in @snapshot: as
/// bch2_inum_snapshot_to_path(). In the caller's transaction attempt.
pub fn inum_snapshot_to_path(
    trans:    &BtreeTrans<'_>,
    inum:     u64,
    snapshot: u32,
    out:      &mut Printbuf,
) -> Result<(), BchError> {
    __inum_to_path(&trans.attempt_in_progress(), 0, inum, snapshot, 0, false, out)
}

/// For C's error reporting and reconcile: bch2_inum_snapshot_to_path(). Its
/// snapshot list was never used.
#[no_mangle]
pub extern "C" fn bch2_inum_snapshot_to_path(
    trans:     &Opaque<c::btree_trans>,
    inum:      u64,
    snapshot:  u32,
    _snapshot_overwrites: *mut c::snapshot_id_list,
    out:       &mut Printbuf,
) -> c_int {
    ret_to_c(inum_snapshot_to_path(&BtreeTrans::from_c(trans), inum, snapshot, out))
}

/// Whether @inum is @ancestor or below it: the walk from inum_to_path(),
/// without the names - up through each inode's directory, crossing into
/// parent subvolumes - until @ancestor or the root. A walk that's
/// disconnected or loops isn't below anything. As bch2_inum_is_descendant().
pub fn inum_is_descendant(
    trans:    &BtreeTrans<'_>,
    inum:     c::subvol_inum,
    ancestor: c::subvol_inum,
) -> Result<bool, BchError> {
    let mut subvol = inum.subvol as u32;
    let mut cur = inum.inum;
    let mut seen: KVVec<(u32, u64)> = KVVec::new();

    let mut snapshot = subvolume::get_snapshot(trans, subvol)?;

    loop {
        if subvol as u64 == ancestor.subvol && cur == ancestor.inum {
            return Ok(true);
        }

        let n = (subvol, cur);
        if seen.contains(&n) {
            return Ok(false);
        }
        seen.push(n, GFP_KERNEL)?;

        let Some(inode) = inode::find_by_inum_snapshot(trans, cur, snapshot,
                                                       BtreeIterFlags::empty()).found()? else {
            return Ok(false);
        };

        if is_root(&inode) || (inode.bi_dir == 0 && inode.bi_dir_offset == 0) {
            return Ok(false);
        }

        if inode.bi_parent_subvol != 0 {
            subvol = inode.bi_parent_subvol;
            snapshot = subvolume::get_snapshot(trans, subvol)?;
        }
        cur = inode.bi_dir;
    }
}

/// For C's ioctls: bch2_inum_is_descendant() - 1 yes, 0 no.
#[no_mangle]
pub extern "C" fn bch2_inum_is_descendant(
    trans:    &Opaque<c::btree_trans>,
    inum:     c::subvol_inum,
    ancestor: c::subvol_inum,
) -> c_int {
    match inum_is_descendant(&BtreeTrans::from_c(trans), inum, ancestor) {
        Ok(yes) => yes as c_int,
        Err(e)  => -e.raw(),
    }
}

// ── A dirent and the inode it names ──────────────────────────────────────

/// Whether @d names @inode - a DT_SUBVOL dirent by subvolume, any other by
/// inode number: as dirent_points_to_inode_nowarn().
pub fn dirent_points_to_inode(d: Dirent<'_>, inode: &c::bch_inode_unpacked) -> bool {
    match d.target() {
        DirentTarget::Subvol { child, .. } => child == inode.bi_subvol,
        DirentTarget::Inode(inum)          => inum == inode.bi_inum,
    }
}

/// Whether @inode's backpointer is @d: as inode_points_to_dirent().
fn inode_points_to_dirent(inode: &c::bch_inode_unpacked, d: Dirent<'_>) -> bool {
    inode.bi_dir        == d.k().k.p.inode &&
    inode.bi_dir_offset == d.k().k.p.offset
}

/// How @k, a dirent, fails to match @inode, which it was expected to point
/// at - for formatting with {}.
pub fn dirent_inode_mismatch<'a, 'k>(
    fs:    &'a Fs,
    k:     BkeySC<'k>,
    inode: &'a c::bch_inode_unpacked,
) -> DirentInodeMismatch<'a, 'k> {
    DirentInodeMismatch { fs, k, inode }
}

pub struct DirentInodeMismatch<'a, 'k> {
    fs:    &'a Fs,
    k:     BkeySC<'k>,
    inode: &'a c::bch_inode_unpacked,
}

impl fmt::Display for DirentInodeMismatch<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "inode points to dirent that does not point back:\n{}\n{}",
               self.k.to_text(self.fs), self.inode)
    }
}

/// Is @d the name of @target, given that @target is a subvolume root?
///
/// A subvolume root's name is not ambiguous: it is the DT_SUBVOL dirent that
/// names its subvolume, and inode::d_type() says so from the other side.
/// dirent_points_to_inode() is not enough to pick it out - for a subvolume
/// root it also accepts a DT_DIR dirent naming bi_inum, which is exactly
/// what a reattach into lost+found manufactures.
fn dirent_is_subvol_root_name(d: Dirent<'_>, target: &c::bch_inode_unpacked) -> bool {
    d.d_type() as u32 == c::DT_SUBVOL && dirent_points_to_inode(d, target)
}

/// @d names @target: check that @target's backpointer is @d, or failing
/// that another dirent naming it, repairing whichever is wrong - as fsck
/// with @in_fsck; otherwise, for a directory or subvolume with two names,
/// the filesystem is inconsistent. As bch2_check_dirent_inode_dirent().
fn check_dirent_inode_dirent(
    t:       &TransAttempt<'_, '_>,
    d:       Dirent<'_>,
    target:  &mut c::bch_inode_unpacked,
    in_fsck: bool,
) -> Result<(), BchError> {
    let fs = t.fs();
    let d_pos = d.k().k.p;

    if inode_points_to_dirent(target, d) {
        return Ok(());
    }

    if !target.has_backpointer() {
        fsck_err_on!(t, target.is_dir(), id::inode_dir_missing_backpointer,
                     "directory with missing backpointer\n{}\n{}",
                     d.k().to_text(fs), target)?;

        fsck_err_on!(t, target.flag(BCH_INODE_unlinked), id::inode_unlinked_but_has_dirent,
                     "inode unlinked but has dirent\n{}\n{}",
                     d.k().to_text(fs), target)?;

        target.set_flag(BCH_INODE_unlinked, false);
        target.bi_dir        = d_pos.inode;
        target.bi_dir_offset = d_pos.offset;
        return inode::fsck_write(t, target);
    }

    let mut bp_iter = BtreeIter::new(t, c::btree_id::dirents,
                                     spos(target.bi_dir, target.bi_dir_offset, target.bi_snapshot),
                                     BtreeIterFlags::empty());
    let bp = bp_iter.peek_slot_typed(t, c::bch_bkey_type::KEY_TYPE_dirent).found()?;

    let Some(bp) = bp else {
        if fsck_err!(t, id::inode_wrong_backpointer,
                     "inode has wrong backpointer:\n\
                      got       {}:{}\n\
                      should be {}:{}\n{}\n{}",
                     target.bi_dir, target.bi_dir_offset,
                     { d_pos.inode }, { d_pos.offset },
                     target, d.k().to_text(fs))? {
            target.bi_dir        = d_pos.inode;
            target.bi_dir_offset = d_pos.offset;
            inode::fsck_write(t, target)?;
        }
        return Ok(());
    };

    let mut buf = Printbuf::new();
    write!(buf, "{}\n{}", d.k().to_text(fs), bp.to_text(fs));

    if target.is_dir() || target.bi_subvol != 0 {
        // Which of the two is wrong?
        //
        // For a subvolume root we can say: its name is the DT_SUBVOL dirent
        // naming its subvolume, so if exactly one of the two qualifies, the
        // other one goes - whichever of them we happen to be holding. A
        // reattach that didn't recognise a subvolume root manufactures a
        // DT_DIR dirent naming bi_inum in lost+found and points the inode's
        // backpointer at it, so the impostor is routinely the one the
        // backpointer names; dropping the dirent in hand would take the real
        // name instead (field report 2026-08-04).
        //
        // If neither or both qualify we're guessing again, so fall back to
        // dropping the dirent in hand.
        //
        // XXX: for a plain directory we still can't tell, and verifying
        // connectivity of the other dirent up to the root before removing
        // this one is still to do.
        //
        // Additionally, bch2_lookup would need to cope with the dirent it
        // found being removed - or should we remove the other one, even
        // though the inode points to it?
        let mut remove = d_pos;
        let mut repoint_backpointer = false;

        if target.bi_subvol != 0 &&
           dirent_is_subvol_root_name(d, target) &&
           !dirent_is_subvol_root_name(Dirent::new(bp).expect("a dirent"), target) {
            remove              = bp.k.p;
            repoint_backpointer = true;

            write!(buf, "\nremoving {}:{}: a subvolume root's name is its DT_SUBVOL dirent",
                   { remove.inode }, { remove.offset });
        }

        let what = if target.is_dir() { "directory" } else { "subvolume" };

        if in_fsck {
            if fsck_err!(t, id::inode_dir_multiple_links,
                         "{} {}:{} with multiple links\n{}",
                         what, target.bi_inum, target.bi_snapshot, buf)? {
                dirent::fsck_remove(t, remove)?;

                // We just removed the dirent the inode named - point it at
                // the survivor, or we've left a dangling backpointer.
                if repoint_backpointer {
                    target.bi_dir        = d_pos.inode;
                    target.bi_dir_offset = d_pos.offset;
                    inode::fsck_write(t, target)?;
                }
            }
        } else {
            fs_inconsistent!(fs, "{} {}:{} with multiple links\n{}",
                             what, target.bi_inum, target.bi_snapshot, buf);
        }
    } else {
        // hardlinked file with nlink 0:
        // We're just adjusting nlink here so check_nlinks() will pick it up,
        // it ignores inodes with nlink 0
        if fsck_err_on!(t, target.bi_nlink == 0, id::inode_multiple_links_but_nlink_0,
                        "inode {}:{} type {} has multiple links but i_nlink 0\n{}",
                        target.bi_inum, target.bi_snapshot,
                        d_type_str(d.d_type()), buf)? {
            target.bi_nlink = target.bi_nlink.wrapping_add(1);
            target.set_flag(BCH_INODE_unlinked, false);
            inode::fsck_write(t, target)?;
        }
    }

    Ok(())
}

/// The slow path of check_dirent_target(): as __bch2_check_dirent_target().
fn __check_dirent_target(
    t:           &TransAttempt<'_, '_>,
    dirent_iter: &BtreeIter<'_>,
    d:           Dirent<'_>,
    target:      &mut c::bch_inode_unpacked,
    in_fsck:     bool,
) -> Result<(), BchError> {
    let fs = t.fs();

    check_dirent_inode_dirent(t, d, target, in_fsck)?;

    let d_type = target.d_type();

    if fsck_err_on!(t, d.d_type() != d_type, id::dirent_d_type_wrong,
                    "incorrect d_type: got {}, should be {}:\n{}",
                    d_type_str(d.d_type()),
                    d_type_str(d_type),
                    d.k().to_text(fs))? {
        let mut n = t.bkey_reassemble(d.k())?;
        let v = n.k_i_mut().as_mut_dirent().expect("a dirent");

        v.set_d_type(d_type);
        if d_type as u32 == c::DT_SUBVOL {
            v.__bindgen_anon_1.__bindgen_anon_1.d_parent_subvol = target.bi_parent_subvol.to_le();
            v.__bindgen_anon_1.__bindgen_anon_1.d_child_subvol  = target.bi_subvol.to_le();
        } else {
            v.__bindgen_anon_1.d_inum = target.bi_inum.to_le();
        }

        t.update(dirent_iter, &n, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)?;
    }

    Ok(())
}

/// Check that @d, a dirent at @iter, and @target, the inode it points at,
/// agree - the inode's backpointer and the dirent's d_type - repairing
/// whichever is wrong, as fsck: as bch2_check_dirent_target(), with
/// @in_fsck.
pub fn check_dirent_target(
    t:       &TransAttempt<'_, '_>,
    iter:    &BtreeIter<'_>,
    d:       Dirent<'_>,
    target:  &mut c::bch_inode_unpacked,
    in_fsck: bool,
) -> Result<(), BchError> {
    if inode_points_to_dirent(target, d) && d.d_type() == target.d_type() {
        return Ok(());
    }

    __check_dirent_target(t, iter, d, target, in_fsck)
}

/// For C's VFS lookup, through namei.h's bch2_check_dirent_target():
/// __bch2_check_dirent_target().
///
/// # Safety
/// @d a dirent from the btree, valid for the call.
#[no_mangle]
pub unsafe extern "C" fn __bch2_check_dirent_target(
    trans:       &Opaque<c::btree_trans>,
    dirent_iter: &mut Opaque<c::btree_iter>,
    d:           c::bkey_s_c_dirent,
    target:      &mut c::bch_inode_unpacked,
    in_fsck:     bool,
) -> c_int {
    let k = c::bkey_s_c::from(d);

    ret_to_c(__check_dirent_target(&BtreeTrans::from_c(trans).attempt_in_progress(),
                                   BtreeIter::from_c(dirent_iter),
                                   Dirent::new(BkeySC::from(&k)).expect("a dirent"),
                                   target, in_fsck))
}

// ── BCH_INODE_has_case_insensitive ───────────────────────────────────────

/// Set BCH_INODE_has_case_insensitive on directory @inum and up from it, as
/// far as a directory that has it already.
fn propagate_has_case_insensitive(t: &TransAttempt<'_, '_>, mut inum: c::subvol_inum)
    -> Result<(), BchError>
{
    loop {
        let mut iter = BtreeIter::uninit();
        let mut inode = inode::peek(t, &mut iter, inum, BtreeIterFlags::empty())?;

        if inode.flag(BCH_INODE_has_case_insensitive) {
            break;
        }

        inode.set_flag(BCH_INODE_has_case_insensitive, true);
        inode::write(t, &mut iter, &mut inode)?;

        if subvol_inum_eq(inum, ROOT_SUBVOL_INUM) {
            break;
        }

        inum = parent_inum(inum, &inode);
    }

    Ok(())
}

/// Directory @inode, which is @inum, was made or moved: if it's casefolded,
/// or has a casefolded directory below it, its parents have to say so - as
/// bch2_maybe_propagate_has_case_insensitive().
pub fn maybe_propagate_has_case_insensitive(
    t:     &TransAttempt<'_, '_>,
    inum:  c::subvol_inum,
    inode: &mut c::bch_inode_unpacked,
) -> Result<(), BchError> {
    if inode.casefold(t.fs()) {
        inode.set_flag(BCH_INODE_has_case_insensitive, true);
    }

    if !inode.flag(BCH_INODE_has_case_insensitive) {
        return Ok(());
    }

    propagate_has_case_insensitive(t, parent_inum(inum, inode))
}

/// Check BCH_INODE_has_case_insensitive on @inode against the casefolded
/// directories on its path, as fsck: as
/// bch2_check_inode_has_case_insensitive(). Sets @do_update if @inode was
/// repaired; ENOENT for an inode that's not connected, which a later pass
/// fixes.
///
/// Only the first parent is checked, unless it was wrong: then the walk goes
/// on up, repairing as it goes, and commits - restarting the caller, to see
/// the repairs.
pub fn check_inode_has_case_insensitive(
    t:         &TransAttempt<'_, '_>,
    inode:     &mut c::bch_inode_unpacked,
    do_update: &mut bool,
) -> Result<(), BchError> {
    let fs = t.fs();
    let mut repairing_parents = false;

    if !inode.is_dir() {
        // Old versions set bi_casefold for non dirs, but that's unnecessary
        // and wasteful
        if inode.bi_casefold != 0 {
            inode.bi_casefold = 0;
            *do_update = true;
        }
        return Ok(());
    }

    if fs.version() <
       c::bcachefs_metadata_version::bcachefs_metadata_version_inode_has_case_insensitive {
        return Ok(());
    }

    if inode.casefold(fs) && !inode.flag(BCH_INODE_has_case_insensitive) {
        let mut buf = Printbuf::new();
        write!(buf,"casefolded dir with has_case_insensitive not set\ninum {}:{} ",
               inode.bi_inum, inode.bi_snapshot);

        inum_snapshot_to_path(t, inode.bi_inum, inode.bi_snapshot, &mut buf)?;

        if fsck_err!(t, id::inode_has_case_insensitive_not_set, "{}", buf)? {
            inode.set_flag(BCH_INODE_has_case_insensitive, true);
            *do_update = true;
        }
    }

    if !inode.flag(BCH_INODE_has_case_insensitive) {
        return Ok(());
    }

    let mut dir = *inode;
    let mut snapshot = dir.bi_snapshot;

    while !is_root(&dir) {
        if dir.bi_parent_subvol != 0 {
            snapshot = subvolume::get_snapshot(t, dir.bi_parent_subvol)?;
        }

        dir = inode::find_by_inum_snapshot(t, dir.bi_dir, snapshot, BtreeIterFlags::empty())?;

        if !dir.flag(BCH_INODE_has_case_insensitive) {
            let mut buf = Printbuf::new();
            writeln!(buf,"parent of casefolded dir with has_case_insensitive not set");

            inum_snapshot_to_path(t, dir.bi_inum, dir.bi_snapshot, &mut buf)?;

            if fsck_err!(t, id::inode_parent_has_case_insensitive_not_set, "{}", buf)? {
                dir.set_flag(BCH_INODE_has_case_insensitive, true);
                inode::fsck_write(t, &mut dir)?;
                repairing_parents = true;
            }
        }

        // We only need to check the first parent, unless we find an
        // inconsistency
        if !repairing_parents {
            break;
        }
    }

    if repairing_parents {
        t.commit(None, CommitFlags::NO_ENOSPC)?;
        return Err(t.restart(bch_errcode::BCH_ERR_transaction_restart_nested));
    }

    Ok(())
}
