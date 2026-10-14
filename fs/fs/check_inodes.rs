// SPDX-License-Identifier: GPL-2.0

//! check_inodes: each inode version on its own - that its fields agree with
//! each other and with what they point at, and that the flags caching facts
//! about other btrees (has_child_snapshot, has_inode_opts, the ACL flags) are
//! right. Repairs edit an unpacked copy, written once at the end.

use crate::acl::AclType;
use crate::btree::bkey::{spos, BkeySC, POS_MIN};
use crate::btree::iter::{
    BtreeIter, BtreeIterFlags, BtreeTrans, CommitFlags, TransAttempt,
};
use crate::c;
use crate::check;
use crate::errcode::{bch_errcode, BchError, Found};
use crate::fs::Fs;
use crate::init::error::id;
use crate::init::progress::Progress;
use crate::snapshots::{snapshot, subvolume};
use crate::util::os_str::{OsStr, OsStrExt};
use crate::util::vstructs::Ffi;
use crate::xattr::{self, Xattrs};
use crate::{bch_err_msg, fsck_err, fsck_err_on, inode_fsck_err};
use crate::dirent::{self, Dirent};
use crate::{inode, namei, str_hash};
use core::fmt;

struct CheckInodes {
    /// The oldest version the current key sees: every version takes its hash
    /// info from it.
    snapshot_root: c::bch_inode_unpacked,
}

/// The dirent @u names must exist and name @u back. Whether @u changed.
fn check_inode_dirent_inode(t: &TransAttempt<'_, '_>, u: &mut c::bch_inode_unpacked) -> Result<bool, BchError> {
    let trans: &BtreeTrans<'_> = t;
    let fs = trans.fs();
    let pos = spos(0, u.bi_inum, u.bi_snapshot);

    let mut snapshot = u.bi_snapshot;
    let mut dirent_iter = BtreeIter::uninit();
    let d = namei::inode_get_dirent(t, &mut dirent_iter, u, &mut snapshot).found()?;
    let points = d.is_some_and(|k| namei::dirent_points_to_inode(Dirent::new(k).expect("a dirent"), u));

    if !points && u.bi_subvol != 0 && u.flag(c::bch_inode_flags::BCH_INODE_has_child_snapshot) {
        // An old version of a renamed subvolume root has no dirent, as
        // expected: the rename can't clear arbitrarily many old backpointers
        u.clear_backpointer();
        return Ok(true);
    }

    let mut changed = false;

    let repair = match d {
        None =>
            inode_fsck_err!(trans, pos, id::inode_points_to_missing_dirent,
                            "inode points to missing dirent\n{u}")?,
        Some(k) if !points =>
            inode_fsck_err!(trans, pos, id::inode_points_to_wrong_dirent,
                            "{}", namei::dirent_inode_mismatch(fs, k, u))?,
        Some(_) => false,
    };
    if repair {
        // check_dirents sets it again if a dirent points here;
        // check_unreachable_inodes reattaches it if none does
        u.clear_backpointer();
        changed = true;
    }

    if let Some(k) = d {
        if points &&
           fsck_err_on!(trans, u.flag(c::bch_inode_flags::BCH_INODE_unlinked), id::inode_unlinked_but_has_dirent,
                        "inode unlinked but has dirent\n{u}\n{}", k.to_text(fs))? {
            // A dirent points here, so the unlinked flag is wrong, and
            // clearing it is the whole repair: nlink becomes 1 (check_nlinks
            // recounts hardlinks), and the trigger drops the deleted_inodes
            // entry
            u.set_flag(c::bch_inode_flags::BCH_INODE_unlinked, false);
            changed = true;
        }
    }

    Ok(changed)
}

/// Whether the inode at @pos is on the deleted_inodes list.
fn on_deleted_list(t: &TransAttempt<'_, '_>, pos: c::bpos) -> Result<bool, BchError> {
    let mut iter = BtreeIter::new(t, c::btree_id::deleted_inodes, pos, BtreeIterFlags::empty());
    Ok(iter.peek_slot(t)?.is_some_and(|k| k.key_type() == c::bch_bkey_type::KEY_TYPE_set))
}

/// What the has_*_acl flags record.
fn has_xattr_type(t: &TransAttempt<'_, '_>, u: &c::bch_inode_unpacked, ty: c::bch_xattr_type)
    -> Result<bool, BchError>
{
    let hash = str_hash::hash_info_init(t.fs(), u)?;
    let key = xattr::XattrKey { ty, name: OsStr::from_bytes(b"") };
    let mut iter = BtreeIter::uninit();

    Ok(str_hash::lookup_in_snapshot::<Xattrs>(t, &mut iter, &hash,
                                              c::subvol_inum { subvol: 0, inum: u.bi_inum },
                                              &key, BtreeIterFlags::empty(), u.bi_snapshot)
       .found()?.is_some())
}

/// A snapshot node, or that there isn't one, for messages.
struct MaybeSnapshot<'a>(Option<&'a Ffi<c::bch_snapshot>>);

impl fmt::Display for MaybeSnapshot<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(s) => write!(f, "{s}"),
            None    => f.write_str("(missing)"),
        }
    }
}

enum SubvolCheck {
    Ok,
    Repaired,
    /// The caller recreates the subvolume key with this root inode, and skips
    /// the rest of the checks.
    Reconstruct(u64),
}

/// The subvolume must exist and point back at this inode, at a snapshot that
/// sees this version.
///
/// Not just at leaves: a snapshotted subvolume's root inode stays at a node
/// that has since become interior, so a leaf check skipped every subvolume ever
/// snapshotted. Live versus stale is whether the subvolume's snapshot descends
/// from this key's (inode_bi_subvol_wrong).
fn check_inode_subvol(
    t:     &TransAttempt<'_, '_>,
    pos:   c::bpos,
    u:     &mut c::bch_inode_unpacked,
) -> Result<SubvolCheck, BchError> {
    let trans: &BtreeTrans<'_> = t;
    let fs = trans.fs();

    let subvol   = subvolume::get(trans, u.bi_subvol, false).found()?;
    let snapshot = snapshot::lookup(trans, u.bi_snapshot).found()?;

    // Reconstruct a missing subvolume if the subvolumes btree lost data, or if
    // the live snapshot this inode is at backrefs the same subvolume: two keys
    // agree, only the subvolume is gone. Not if the snapshot is will_delete:
    // that's a deletion in flight, which reconstructing would revert. Anything
    // else strips the reference, below.
    let snapshot_agrees = snapshot.as_ref().is_some_and(|s| {
        s.state() == Some(c::bch_snapshot_state::SNAPSHOT_STATE_live) &&
            s.subvol.get() == u.bi_subvol
    });

    if subvol.is_none() && (fs.btree_lost_data(c::btree_id::subvolumes) || snapshot_agrees) {
        let root = check::reconstruct_subvol_root(t, pos.snapshot, u.bi_subvol, Some(u.bi_inum))?;
        return Ok(SubvolCheck::Reconstruct(root));
    }

    // No exemption for a missing subvolume: an unlinked one still resolves, and
    // a tombstone's snapshot is will_delete, which we never get here for. Left
    // alone, check_unreachable_inodes() fails on an orphan whose bi_subvol
    // points nowhere (field report, 2026-07-21).
    //
    // Except the root subvolume, which always exists (check_root() recreates
    // it): stripping bi_subvol would reattach the root directory in lost+found.
    if subvol.is_none() && u.bi_subvol == c::BCACHEFS_ROOT_SUBVOL {
        return Ok(SubvolCheck::Ok);
    }

    let snap = MaybeSnapshot(snapshot.as_ref());
    let repair = match subvol {
        None =>
            fsck_err!(trans, id::inode_bi_subvol_missing,
                      "inode bi_subvol points to missing subvolume {}\n{u}\nsnapshot {}: {snap}",
                      u.bi_subvol, u.bi_snapshot)?,
        Some(s) =>
            fsck_err_on!(trans,
                         s.inode.get() != u.bi_inum ||
                         !snapshot::is_ancestor(trans, s.snapshot.get(), pos.snapshot),
                         id::inode_bi_subvol_wrong,
                         "inode points to subvol {}, but subvol points to {}:{}\n{u}\nsnapshot {}: {snap}",
                         u.bi_subvol, s.inode.get(), s.snapshot.get(),
                         u.bi_snapshot)?,
    };
    if !repair {
        return Ok(SubvolCheck::Ok);
    }

    u.bi_subvol        = 0;
    u.bi_parent_subvol = 0;
    Ok(SubvolCheck::Repaired)
}

/// An unlinked directory is allowed if it's an unlinked subvolume's root -
/// the snapshot sweep deletes its contents - or empty: rmdir'd, and still open
/// or not yet reaped, on the deleted list like any unlinked file. Non-empty is
/// damage: clear the flag, so check_unreachable_inodes() reattaches it. Whether
/// @u changed.
fn check_unlinked_dir(
    t:     &TransAttempt<'_, '_>,
    pos:   c::bpos,
    u:     &mut c::bch_inode_unpacked,
) -> Result<bool, BchError> {
    let trans: &BtreeTrans<'_> = t;
    if u.is_subvolume_root() && subvolume::is_unlinked(trans, u.bi_subvol)? {
        return Ok(false);
    }

    match dirent::empty_dir_snapshot(t, pos.offset, 0, pos.snapshot) {
        Ok(()) => return Ok(false),
        Err(e) if e.matches(bch_errcode::BCH_ERR_ENOTEMPTY_dir_not_empty) => {}
        Err(e) => return Err(e),
    }

    if !inode_fsck_err!(trans, pos, id::inode_dir_unlinked_but_not_empty,
                        "dir unlinked but not empty\n{u}")? {
        return Ok(false);
    }

    u.set_flag(c::bch_inode_flags::BCH_INODE_unlinked, false);
    Ok(true)
}

fn write_if_changed(
    t:       &TransAttempt<'_, '_>,
    u:       &mut c::bch_inode_unpacked,
    changed: bool,
) -> Result<(), BchError> {
    if !changed {
        return Ok(());
    }
    let fs = t.trans().fs();
    bch_err_msg!(fs, inode::fsck_write(t, u), "in fsck updating inode")
}

fn check_inode<'t>(
    t:    &TransAttempt<'_, 't>,
    iter: &BtreeIter<'t>,
    k:    BkeySC<'_>,
    st:   &mut CheckInodes,
) -> Result<(), BchError> {
    let trans = t.trans();
    let fs = trans.fs();

    if snapshot::check_key_has_snapshot(trans, iter, k)? {
        return Ok(());
    }

    if !inode::bkey_is_inode(k.k) {
        return Ok(());
    }

    let pos = k.k.p;
    let mut u = inode::unpack(fs, k);
    assert_eq!(u.bi_snapshot, pos.snapshot);

    if st.snapshot_root.bi_inum != u.bi_inum ||
       !snapshot::is_ancestor(trans, u.bi_snapshot, st.snapshot_root.bi_snapshot) {
        st.snapshot_root = inode::find_oldest_snapshot(trans, u.bi_inum, u.bi_snapshot)?;
    }

    if u.bi_hash_seed != st.snapshot_root.bi_hash_seed ||
       u.inode_str_hash() != st.snapshot_root.inode_str_hash() {
        str_hash::repair_inode_hash_info(t, &mut u, &st.snapshot_root)?;
    }

    let mut changed = false;

    // ENOENT: a disconnected inode, which a later pass fixes
    bch_err_msg!(fs, namei::check_inode_has_case_insensitive(t, &mut u, &mut changed)
                 .found(), "bch2_check_inode_has_case_insensitive()")?;

    // Before the backpointer check: the dirent is looked up through
    // bi_parent_subvol, and a bogus one makes a good backpointer look broken.
    if fsck_err_on!(trans, u.bi_parent_subvol != 0 &&
                    (u.bi_subvol == 0 || u.bi_subvol == c::BCACHEFS_ROOT_SUBVOL),
                    id::inode_bi_parent_nonzero,
                    "inode has nonzero bi_parent_subvol but is not a subvolume root\n{u}")? {
        u.bi_parent_subvol = 0;
        changed = true;
    }

    if u.has_backpointer() {
        changed |= check_inode_dirent_inode(t, &mut u)?;
    }

    if u.is_dir() && u.flag(c::bch_inode_flags::BCH_INODE_unlinked) {
        changed |= check_unlinked_dir(t, pos, &mut u)?;
    }

    if fsck_err_on!(trans, u.is_dir() && u.bi_size != 0, id::inode_dir_has_nonzero_i_size,
                    "directory with nonzero i_size\n{u}")? {
        u.bi_size = 0;
        changed = true;
    }

    let has_child = inode::has_child_snapshots(trans, pos)?;
    if fsck_err_on!(trans, has_child != u.flag(c::bch_inode_flags::BCH_INODE_has_child_snapshot),
                    id::inode_has_child_snapshots_wrong,
                    "inode has_child_snapshots flag wrong (should be {})\n{u}", has_child as u32)? {
        u.set_flag(c::bch_inode_flags::BCH_INODE_has_child_snapshot, has_child);
        changed = true;
    }

    // An unlinked subvolume root is the subvolume path's to delete -
    // check_subvols() resumes it - not the inode reaper's
    if u.flag(c::bch_inode_flags::BCH_INODE_unlinked) &&
       !u.is_subvolume_root() &&
       !u.flag(c::bch_inode_flags::BCH_INODE_has_child_snapshot) {
        if !fs.flag(c::bch_fs_flags::BCH_FS_started) {
            // Offline, only make sure it's on the deleted list: a logged
            // operation may still refer to it - a crash mid-truncate of an open
            // unlinked file - so delete_dead_inodes runs after logged ops
            // resume. (The online arm isn't reached: check_inodes isn't
            // PASS_ONLINE.)
            if !on_deleted_list(t, pos)? &&
               fsck_err!(trans, id::unlinked_inode_not_on_deleted_list,
                         "inode unlinked, but not on deleted list\n{u}")? {
                t.bit_mod_buffered(c::btree_id::deleted_inodes, pos, true)?;
            }
        } else if !inode::or_descendents_is_open(trans, pos)? &&
                  fsck_err!(trans, id::inode_unlinked_and_not_open,
                            "inode unlinked and not open\n{u}")? {
            bch_err_msg!(fs, inode::rm_snapshot(trans, u.bi_inum, pos.snapshot),
                         "in fsck deleting inode")?;
            return Ok(());
        }
    }

    let has_opts = u.has_opts();
    if fsck_err_on!(trans, u.flag(c::bch_inode_flags::BCH_INODE_has_inode_opts) != has_opts,
                    id::inode_has_inode_opts_flag_wrong,
                    "inode has_inode_opts flag wrong, should be {}\n{u}", has_opts as u32)? {
        u.set_flag(c::bch_inode_flags::BCH_INODE_has_inode_opts, has_opts);
        changed = true;
    }

    // After the flag check: the flag gates the walk
    inode::check_opts_propagated(trans, &mut u)?;

    // Only flag set, xattr missing: a lookup per flagged inode, which are
    // rare. Flag clear with an xattr - which breaks bch2_get_acl()'s short
    // circuit - is check_xattrs'.
    for acl in AclType::ALL {
        if u.flag(acl.inode_flag()) &&
           fsck_err_on!(trans, !has_xattr_type(t, &u, acl.xattr_type())?, acl.flag_wrong(),
                        "inode has BCH_INODE_has_{}_acl set but no acl xattr\n{u}", acl.name())? {
            u.set_flag(acl.inode_flag(), false);
            changed = true;
        }
    }

    if u.bi_subvol != 0 {
        match check_inode_subvol(t, pos, &mut u)? {
            SubvolCheck::Ok                => {}
            SubvolCheck::Repaired          => changed = true,
            SubvolCheck::Reconstruct(root) => {
                check::reconstruct_subvol(t, pos.snapshot, u.bi_subvol, root)?;
                return write_if_changed(t, &mut u, changed);
            }
        }
    }

    let cur_seq = fs.journal_cur_seq();
    if fsck_err_on!(trans, u.bi_journal_seq > cur_seq, id::inode_journal_seq_in_future,
                    "inode journal seq in future (currently at {cur_seq})\n{u}")? {
        u.bi_journal_seq = cur_seq;
        changed = true;
    }

    write_if_changed(t, &mut u, changed)
}

fn check_inodes(fs: &Fs) -> Result<(), BchError> {
    let trans = crate::btree_trans!(fs);
    let progress = Progress::recovery(fs, c"bch2_check_inodes", &[c::btree_id::inodes], &[]);
    let mut st = CheckInodes {
        snapshot_root: Default::default(),
    };

    let mut iter = BtreeIter::new(&trans, c::btree_id::inodes, POS_MIN,
                                  BtreeIterFlags::PREFETCH | BtreeIterFlags::ALL_SNAPSHOTS);

    iter.for_each_commit(&trans, None, CommitFlags::NO_ENOSPC, |t, iter, k| {
        progress.update(t, iter)?;
        check_inode(t, iter, k, &mut st)
    })
}

crate::recovery_pass!(bch2_check_inodes => check_inodes);
