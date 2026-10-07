// SPDX-License-Identifier: GPL-2.0

//! fsck: the check_inodes recovery pass - each inode key, every snapshot
//! version, on its own: that its fields agree with each other and with what
//! they point at, and that what's derived from them - flags caching the
//! answer to a question about other btrees - is right.
//!
//! In order:
//!  - hash info (seed and type) agrees with the version at the snapshot root;
//!  - has_case_insensitive agrees with the casefolded directories above it;
//!  - bi_parent_subvol is only set on a subvolume root;
//!  - the backpointer (bi_dir, bi_dir_offset) names a dirent that points back;
//!  - an unlinked directory is empty (rmdir'd, not yet reaped), or the root of
//!    an unlinked subvolume;
//!  - a directory has no i_size;
//!  - has_child_snapshot agrees with the versions in descendant snapshots;
//!  - an unlinked inode is on the deleted list (offline), or open (online);
//!  - has_inode_opts agrees with the options set, and they've propagated;
//!  - has_access_acl/has_default_acl have the ACL xattr they claim (the other
//!    direction - an ACL xattr with the flag clear - is check_xattrs');
//!  - bi_subvol names a subvolume that names this inode back;
//!  - bi_journal_seq isn't in the future.
//!
//! Repairs edit an unpacked copy, written once at the end.
//!
//! Changes from the C: a declined repair changes nothing - the C cleared a
//! non-empty directory's unlinked flag and set the deleted_inodes bit whether
//! or not the fix was taken - and a missing subvolume no longer has its
//! uninitialized value compared against the inode when inode_bi_subvol_missing
//! is declined. And bi_parent_subvol is checked before the backpointer, which
//! is looked up through it: the C cleared a good backpointer for a bogus one,
//! for check_dirents to put back.

use crate::btree::bkey::{spos, BkeySC, POS_MIN};
use crate::btree::iter::{
    BtreeIter, BtreeIterFlags, BtreeTrans, CommitFlags, TransAttempt,
};
use crate::c;
use crate::c::bch_inode_flags::{
    BCH_INODE_has_access_acl, BCH_INODE_has_child_snapshot, BCH_INODE_has_default_acl,
    BCH_INODE_has_inode_opts, BCH_INODE_unlinked,
};
use crate::check;
use crate::errcode::{bch_errcode, BchError, Found};
use crate::fs::Fs;
use crate::init::error::id;
use crate::init::progress::Progress;
use crate::snapshots::{snapshot, subvolume};
use crate::util::os_str::{OsStr, OsStrExt};
use crate::xattr::{self, Xattrs};
use crate::{bch_err_msg, fsck_err, fsck_err_on, inode_fsck_err};
use crate::dirent::{self, Dirent};
use crate::{inode, namei, str_hash};
use core::fmt;

struct CheckInodes {
    /// The oldest version of the inode being walked that the current key
    /// sees - the one every version takes its hash info from.
    snapshot_root: c::bch_inode_unpacked,
}

/// The backpointer: the dirent @u names has to exist and name @u back.
/// Returns whether @u was changed.
fn check_inode_dirent_inode(t: &TransAttempt<'_, '_>, u: &mut c::bch_inode_unpacked) -> Result<bool, BchError> {
    let trans: &BtreeTrans<'_> = t;
    let fs = trans.fs();
    let pos = spos(0, u.bi_inum, u.bi_snapshot);

    let mut snapshot = u.bi_snapshot;
    let mut dirent_iter = BtreeIter::uninit();
    let d = namei::inode_get_dirent(t, &mut dirent_iter, u, &mut snapshot).found()?;
    let points = d.is_some_and(|k| namei::dirent_points_to_inode(Dirent::new(k).expect("a dirent"), u));

    if !points && u.bi_subvol != 0 && u.flag(BCH_INODE_has_child_snapshot) {
        // Older version of a renamed subvolume root: we won't have a correct
        // dirent for it. That's expected, see inode_should_reattach().
        //
        // We don't clear the backpointer field when doing the rename because
        // there might be arbitrarily many versions in older snapshots.
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
        // We just clear the backpointer fields for now. If we find a dirent
        // that points to this inode in check_dirents(), we'll update it then;
        // then when we get to check_path() if the backpointer is still 0
        // we'll reattach it.
        u.clear_backpointer();
        changed = true;
    }

    if let Some(k) = d {
        if points &&
           fsck_err_on!(trans, u.flag(BCH_INODE_unlinked), id::inode_unlinked_but_has_dirent,
                        "inode unlinked but has dirent\n{u}\n{}", k.to_text(fs))? {
            // The dirent was just verified to point at this inode, so the
            // unlinked flag is wrong - and the flag clear is the complete
            // repair: bi_nlink counts links beyond the first, so this yields
            // nlink 1 (check_nlinks recounts hardlinks), and the inode trigger
            // removes the deleted_inodes entry.
            u.set_flag(BCH_INODE_unlinked, false);
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

/// Whether @u has an xattr of @x_type: what the has_*_acl flags record.
fn has_xattr_type(t: &TransAttempt<'_, '_>, u: &c::bch_inode_unpacked, x_type: u32)
    -> Result<bool, BchError>
{
    let hash = str_hash::hash_info_init(t.fs(), u)?;
    let key = xattr::search_key(x_type, OsStr::from_bytes(b""));
    let mut iter = BtreeIter::uninit();

    Ok(str_hash::lookup_in_snapshot::<Xattrs>(t, &mut iter, &hash,
                                              c::subvol_inum { subvol: 0, inum: u.bi_inum },
                                              &key, BtreeIterFlags::empty(), u.bi_snapshot)
       .found()?.is_some())
}

/// A snapshot node, or that there isn't one, for messages.
struct MaybeSnapshot<'a>(Option<&'a c::bch_snapshot>);

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
    /// The subvolume key needs recreating, with this root inode - the caller
    /// has the attempt to do it with - and the C skips the rest of the
    /// checks.
    Reconstruct(u64),
}

/// bi_subvol: the subvolume has to exist and point back at this inode, at a
/// snapshot that sees this version.
///
/// Not gated on the snapshot being a leaf: taking a snapshot rewrites the root
/// inode of the new subvolume, not of the old, so a live subvolume's root
/// inode key stays at a node that has since become interior. Leaf-ness
/// therefore skipped this check for every subvolume that had ever been
/// snapshotted - so a lost subvolume key was never reconstructed and bi_subvol
/// was never validated for exactly the subvolumes with the most history behind
/// them. Live versus stale is decided by inode_bi_subvol_wrong instead: the
/// subvolume's snapshot has to have this key's snapshot as an ancestor, which
/// is the actual question leaf-ness was standing in for.
fn check_inode_subvol(
    t:     &TransAttempt<'_, '_>,
    pos:   c::bpos,
    u:     &mut c::bch_inode_unpacked,
) -> Result<SubvolCheck, BchError> {
    let trans: &BtreeTrans<'_> = t;
    let fs = trans.fs();

    let subvol   = subvolume::get(trans, u.bi_subvol, false).found()?;
    let snapshot = snapshot::lookup(trans, u.bi_snapshot).found()?;

    // A missing subvolume is reconstructed in two cases. If the subvolumes
    // btree is known to have lost data, reconstruct. Or if this root inode
    // names the subvolume and the live snapshot it lives at names the same
    // subvolume in its backref, reconstruct: two keys point at the edge and
    // only the subvolume key is gone.
    //
    // If the snapshot is will_delete, don't - there the missing subvolume is
    // a deletion in flight, and reconstructing it live would revert the
    // deletion; the sweep owns those keys. Anything else falls through to the
    // conservative arms below, which strip the reference.
    let snapshot_agrees = snapshot.as_ref().is_some_and(|s| {
        s.state() == Some(c::bch_snapshot_state::SNAPSHOT_STATE_live) &&
            u32::from_le(s.subvol) == u.bi_subvol
    });

    if subvol.is_none() && (fs.btree_lost_data(c::btree_id::subvolumes) || snapshot_agrees) {
        let root = check::reconstruct_subvol_root(t, pos.snapshot, u.bi_subvol, Some(u.bi_inum))?;
        return Ok(SubvolCheck::Reconstruct(root));
    }

    // No exemption for a missing subvolume: an unlinked subvolume still
    // resolves, and once it's tombstoned the snapshot is will_delete and we
    // never visit these keys - the sweep owns them. A root inode pointing at a
    // subvolume that's actually gone is damage, and the repair below must run
    // or check_unreachable_inodes() meets an orphan whose bi_subvol points
    // nowhere and the reattach fail-stops the pass (field report, 2026-07-21).
    //
    // A reference to the root subvolume is never the broken side: subvol 1's
    // existence is an invariant, and check_root() recreates it if lost.
    // Stripping bi_subvol around the absence takes a valid backref off the
    // root inode - and check_unreachable_inodes() then reattaches the root
    // directory into lost+found:
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
                         u64::from_le(s.inode) != u.bi_inum ||
                         !snapshot::is_ancestor(trans, u32::from_le(s.snapshot), pos.snapshot),
                         id::inode_bi_subvol_wrong,
                         "inode points to subvol {}, but subvol points to {}:{}\n{u}\nsnapshot {}: {snap}",
                         u.bi_subvol, u64::from_le(s.inode), u32::from_le(s.snapshot),
                         u.bi_snapshot)?,
    };
    if !repair {
        return Ok(SubvolCheck::Ok);
    }

    u.bi_subvol        = 0;
    u.bi_parent_subvol = 0;
    Ok(SubvolCheck::Repaired)
}

/// BCH_INODE_unlinked on a directory is allowed on the root of an unlinked
/// subvolume - legitimately non-empty, since the snapshot sweep is what
/// deletes the contents - and on an empty directory: rmdir sets it
/// (bch2_inode_nlink_dec(); bi_nlink is the subdirectory count), and one
/// that's still open, or not yet reaped at a crash, is waiting for deletion
/// like any unlinked file - and is handled as one, on the deleted list.
///
/// A non-empty one is damage, rmdir only taking empty directories: clear the
/// flag, so check_unreachable_inodes() reattaches it. Returns whether @u was
/// changed.
fn check_unlinked_dir(
    t:     &TransAttempt<'_, '_>,
    pos:   c::bpos,
    u:     &mut c::bch_inode_unpacked,
) -> Result<bool, BchError> {
    let trans: &BtreeTrans<'_> = t;
    if inode::is_subvolume_root(u) && subvolume::is_unlinked(trans, u.bi_subvol)? {
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

    u.set_flag(BCH_INODE_unlinked, false);
    Ok(true)
}

/// The ACL flags, with the xattr each records and its fsck error.
const ACL_FLAGS: [(c::bch_inode_flags, u32, c::bch_sb_error_id, &str); 2] = [
    (BCH_INODE_has_access_acl,  c::KEY_TYPE_XATTR_INDEX_POSIX_ACL_ACCESS,
     id::inode_has_access_acl_flag_wrong,  "access"),
    (BCH_INODE_has_default_acl, c::KEY_TYPE_XATTR_INDEX_POSIX_ACL_DEFAULT,
     id::inode_has_default_acl_flag_wrong, "default"),
];

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
       u.str_hash()   != st.snapshot_root.str_hash() {
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

    if inode::has_backpointer(&u) {
        changed |= check_inode_dirent_inode(t, &mut u)?;
    }

    if u.is_dir() && u.flag(BCH_INODE_unlinked) {
        changed |= check_unlinked_dir(t, pos, &mut u)?;
    }

    if fsck_err_on!(trans, u.is_dir() && u.bi_size != 0, id::inode_dir_has_nonzero_i_size,
                    "directory with nonzero i_size\n{u}")? {
        u.bi_size = 0;
        changed = true;
    }

    let has_child = inode::has_child_snapshots(trans, pos)?;
    if fsck_err_on!(trans, has_child != u.flag(BCH_INODE_has_child_snapshot),
                    id::inode_has_child_snapshots_wrong,
                    "inode has_child_snapshots flag wrong (should be {})\n{u}", has_child as u32)? {
        u.set_flag(BCH_INODE_has_child_snapshot, has_child);
        changed = true;
    }

    // Unlinked subvolume roots are skipped here: their deletion belongs to the
    // subvolume path (check_subvols() resumes it after a crash), not the inode
    // reaper or the deleted_inodes btree:
    if u.flag(BCH_INODE_unlinked) &&
       !inode::is_subvolume_root(&u) &&
       !u.flag(BCH_INODE_has_child_snapshot) {
        if !fs.flag(c::bch_fs_flags::BCH_FS_started) {
            // If we're not in online fsck, don't delete unlinked inodes, just
            // make sure they're on the deleted list.
            //
            // They might be referred to by a logged operation - i.e. we might
            // have crashed in the middle of a truncate on an unlinked but open
            // file - so we want to let the delete_dead_inodes kill it after
            // resuming logged ops.
            //
            // (The online arm below isn't reached today: check_inodes isn't
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
    if fsck_err_on!(trans, u.flag(BCH_INODE_has_inode_opts) != has_opts,
                    id::inode_has_inode_opts_flag_wrong,
                    "inode has_inode_opts flag wrong, should be {}\n{u}", has_opts as u32)? {
        u.set_flag(BCH_INODE_has_inode_opts, has_opts);
        changed = true;
    }

    // after the flag check above: it's what gates the walk
    inode::check_opts_propagated(trans, &mut u)?;

    // has_access_acl/has_default_acl: only the set direction is verified
    // here, an xattr lookup per flagged inode - inodes with ACLs are rare.
    // Flag clear but xattr present, the direction that would break the
    // bch2_get_acl() short circuit, is caught from the xattr side in
    // check_xattrs:
    for (flag, x_type, err, acl) in ACL_FLAGS {
        if u.flag(flag) &&
           fsck_err_on!(trans, !has_xattr_type(t, &u, x_type)?, err,
                        "inode has BCH_INODE_has_{acl}_acl set but no acl xattr\n{u}")? {
            u.set_flag(flag, false);
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
