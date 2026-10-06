// SPDX-License-Identifier: GPL-2.0

use crate::btree::bkey::BkeySC;
use crate::btree::iter::{BtreeIter, BtreeTrans, TransAttempt};
use crate::c;
use crate::check::SnapshotsSeen;
use crate::dirent::{Dirent, DirentTarget};
use crate::errcode::{ret_to_result_void, BchError};
use crate::fs::Fs;
use crate::util::os_str::{qstr, OsStr};
use crate::util::Printbuf;
use core::fmt;

/// Check BCH_INODE_has_case_insensitive on @inode against the casefolded
/// directories on its path, as fsck: as bch2_check_inode_has_case_insensitive().
/// @s's IDs are the snapshots seen at @inode's position. Sets @do_update if
/// @inode was repaired; ENOENT for an inode that's not connected, which a
/// later pass fixes.
pub fn check_inode_has_case_insensitive(
    trans:     &BtreeTrans<'_>,
    inode:     &mut c::bch_inode_unpacked,
    s:         &mut SnapshotsSeen,
    do_update: &mut bool,
) -> Result<(), BchError> {
    let mut ids = s.ids_view();
    ret_to_result_void(unsafe {
        c::bch2_check_inode_has_case_insensitive(trans.raw(), inode, &mut ids, do_update)
    })
}

/// Print @inum's path, through its dirents: as bch2_inum_to_path().
pub fn inum_to_path(
    trans: &BtreeTrans<'_>,
    inum:  c::subvol_inum,
    out:   &mut Printbuf,
) -> Result<(), BchError> {
    ret_to_result_void(unsafe { c::bch2_inum_to_path(trans.raw(), inum, out.as_raw()) })
}

/// Print the path of inode @inum as seen in @snapshot: as
/// bch2_inum_snapshot_to_path(), without collecting the snapshots it passes
/// through.
pub fn inum_snapshot_to_path(
    trans:    &BtreeTrans<'_>,
    inum:     u64,
    snapshot: u32,
    out:      &mut Printbuf,
) -> Result<(), BchError> {
    ret_to_result_void(unsafe {
        c::bch2_inum_snapshot_to_path(trans.raw(), inum, snapshot, core::ptr::null_mut(),
                                      out.as_raw())
    })
}

pub fn link_trans<'a, 't>(
    t:        &TransAttempt<'a, 't>,
    dir_inum: c::subvol_inum,
    dir:      &mut c::bch_inode_unpacked,
    inum:     c::subvol_inum,
    inode:    &mut c::bch_inode_unpacked,
    name:     &OsStr,
) -> Result<(), BchError> {
    let ret = unsafe {
        c::bch2_link_trans(t.raw(), dir_inum, dir, inum, inode, &qstr(name))
    };
    t.result(ret)
}

pub fn unlink_trans<'a, 't>(
    t:        &TransAttempt<'a, 't>,
    dir_inum: c::subvol_inum,
    dir:      &mut c::bch_inode_unpacked,
    target:   c::subvol_inum,
    inode:    &mut c::bch_inode_unpacked,
    name:     &OsStr,
    deleting: bool,
) -> Result<(), BchError> {
    let ret = unsafe {
        c::bch2_unlink_trans(t.raw(), dir_inum, dir, target, inode, &qstr(name), deleting)
    };
    t.result(ret)
}

/// The opt changes are for rename_opt_changes_finish(), after the commit.
#[allow(clippy::too_many_arguments)]
pub fn rename_trans<'a, 't>(
    t:              &TransAttempt<'a, 't>,
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
    let ret = unsafe {
        c::bch2_rename_trans(
            t.raw(),
            src_dir,
            src_dir_u,
            dst_dir,
            dst_dir_u,
            src_inode_u,
            dst_inode_u,
            &qstr(src_name),
            &qstr(dst_name),
            mode,
            src_opt_change,
            dst_opt_change,
        )
    };
    t.result(ret)
}

/// After rename_trans() has committed: see bch2_inode_opt_change_finish().
pub fn rename_opt_changes_finish(
    fs:  &Fs,
    src: &mut c::inode_opt_change,
    dst: &mut c::inode_opt_change,
) -> Result<(), BchError> {
    let trans = crate::btree_trans!(fs);
    ret_to_result_void(unsafe { c::bch2_inode_opt_change_finish(trans.raw(), src) })?;
    ret_to_result_void(unsafe { c::bch2_inode_opt_change_finish(trans.raw(), dst) })
}

#[allow(clippy::too_many_arguments)]
pub fn create_trans<'a, 't>(
    t:            &TransAttempt<'a, 't>,
    dir_inum:     c::subvol_inum,
    dir:          &mut c::bch_inode_unpacked,
    inode:        &mut c::bch_inode_unpacked,
    subvol:       &mut c::bch_subvolume,
    name:         &OsStr,
    uid:          c::uid_t,
    gid:          c::gid_t,
    mode:         c::umode_t,
    rdev:         c::dev_t,
    snapshot_src: c::subvol_inum,
    flags:        u32,
) -> Result<(), BchError> {
    let ret = unsafe {
        c::bch2_create_trans(
            t.raw(),
            dir_inum,
            dir,
            inode,
            subvol,
            &qstr(name),
            uid,
            gid,
            mode,
            rdev,
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            snapshot_src,
            flags,
        )
    };
    t.result(ret)
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

/// Check that @k, a dirent at @iter, and @target, the inode it points at,
/// agree - the inode's backpointer and the dirent's d_type - repairing
/// whichever is wrong, as fsck: as bch2_check_dirent_target().
pub fn check_dirent_target(
    trans:  &BtreeTrans<'_>,
    iter:   &BtreeIter<'_>,
    k:      BkeySC<'_>,
    target: &mut c::bch_inode_unpacked,
) -> Result<(), BchError> {
    let d = k.to_c_dirent().expect("a dirent");
    ret_to_result_void(unsafe {
        c::bch2_check_dirent_target(trans.raw(), iter.raw(), d, target, true)
    })
}
