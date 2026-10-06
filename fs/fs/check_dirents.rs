// SPDX-License-Identifier: GPL-2.0

//! fsck: the check_dirents recovery pass - every dirent belongs to a live
//! directory in its snapshot, sits at the offset its hash says, and points at
//! something that exists and agrees with it: an inode whose backpointer and
//! type match, or for DT_SUBVOL a subvolume whose root and fs_path_parent
//! match. Along the way it counts each directory's subdirectories, and checks
//! i_nlink against the count once the walk leaves the directory.
//!
//! Two inode walkers: @dir walks the directories the dirents belong to, in
//! key order (InodeWalker::walk()); @target is loaded per dirent with the
//! versions of the inode it points at that are visible from it
//! (InodeWalker::get_visible()).
//!
//! Snapshots make "points at something that exists" subtle. The target inode
//! must exist in an ancestor snapshot of the dirent, so every subvolume that
//! sees the dirent sees the inode; one that exists only in descendant
//! snapshots gets the dirent moved down to them. And a target inode deleted
//! in a descendant snapshot (a whiteout there) must not stay reachable from
//! that snapshot through an older dirent: those get whiteouts too.
//!
//! The subdirectory counts are kept per dir walker entry, incremented after
//! each DT_DIR dirent commits. When the walker refetches partway through a
//! directory (a commit changed inodes, or it was invalidated), the counts are
//! partial and are recounted from the btree instead (recalculate_sums).
//!
//! Moving a dirent down to descendant snapshots puts it ahead of the walk's
//! position, so the pass runs a second time to check the copies; a second
//! pass that still finds work is an error.

use crate::btree::bkey::{pos, spos, BkeySC, POS_MIN};
use crate::btree::iter::{
    is_restart, lockrestart_do, BtreeIter, BtreeIterFlags, BtreeTrans, CommitFlags, TransAttempt,
    UpdateTriggerFlags,
};
use crate::c;
use crate::check::{self, InodeWalker, SnapshotsSeen};
use crate::dirent::{self, Dirent, DirentTarget, Dirents};
use crate::errcode::{bch_errcode, BchError, Found};
use crate::fs::Fs;
use crate::init::error::id;
use crate::init::progress::Progress;
use crate::snapshots::{snapshot, subvolume};
use crate::util::Printbuf;
use crate::{bch_err, bch_err_fn, bch_err_ratelimited, bch_info, fsck_err, fsck_err_on, inode_fsck_err};
use crate::{inode, namei, str_hash};
use core::ops::ControlFlow;

struct CheckDirents {
    s:                SnapshotsSeen,
    dir:              InodeWalker,
    target:           InodeWalker,
    hash_info:        c::bch_hash_info,
    need_second_pass: bool,
}

// ---------------------------------------------------------------------------
// Subdirectory counts

/// The subdirectories of directory @inum, as its version in @snapshot sees
/// them. In the caller's attempt - a restart is returned, not retried - as it
/// runs self_committing() (see check_dir_nlink()).
fn count_subdirs(t: &TransAttempt<'_, '_>, inum: u64, snapshot: u32) -> Result<u64, BchError> {
    let mut subdirs = 0;
    let mut iter = BtreeIter::new(t, c::btree_id::dirents, spos(inum, 0, snapshot),
                                  BtreeIterFlags::empty());

    iter.for_each_max_norestart(t, pos(inum, u64::MAX), |_, k| {
        if Dirent::new(k).is_some_and(|d| d.d_type() as u32 == c::DT_DIR) {
            subdirs += 1;
        }
        Ok(())
    })?;

    Ok(subdirs)
}

/// Check one version of directory @inum, @i, against the subdirectories
/// counted for it - recounting from the btree if the count disagrees, or
/// is known to be partial (@recalculate_sums) - and repair i_nlink.
fn check_dir_nlink(
    t:                &TransAttempt<'_, '_>,
    inum:             u64,
    recalculate_sums: bool,
    i:                &mut check::WalkerEntry,
) -> Result<(), BchError> {
    let trans: &BtreeTrans<'_> = t;
    let fs = trans.fs();

    if i.inode.bi_nlink as u64 == i.count {
        return Ok(());
    }

    let snapshot = i.inode.bi_snapshot;
    let count = count_subdirs(t, inum, snapshot)?;

    if !recalculate_sums && i.count != count {
        bch_err_ratelimited!(fs, "fsck counted subdirectories wrong for inum {inum}:{snapshot}: got {} should be {count}",
                             i.count);
    }
    i.count = count;

    if i.inode.bi_nlink as u64 == i.count {
        return Ok(());
    }

    let mut path = Printbuf::new();
    // The path is for the message: failing to find it isn't an error. Looked
    // up in the caller's attempt, a restart returned: this runs
    // self_committing(), where beginning an attempt means the key is retried
    // - and a declined repair, retried, would find the same count and begin
    // another, forever.
    if let Err(e) = namei::inum_snapshot_to_path(trans, inum, snapshot, &mut path) {
        if is_restart(&e) {
            return Err(e);
        }
    }

    if fsck_err!(trans, id::inode_dir_wrong_nlink,
                 "directory with wrong i_nlink: got {}, should be {}\n{path}\n{}",
                 i.inode.bi_nlink, i.count, i.inode)? {
        i.inode.bi_nlink = i.count as u32;
        inode::fsck_write_inode(trans, &mut i.inode)?;
    }

    Ok(())
}

/// Check i_nlink of each version of the directory @w has walked against its
/// subdirectory count. The repairs commit for themselves, so this runs
/// self_committing() - see check_subdir_dirents_count().
fn check_subdir_count_notnested(t: &TransAttempt<'_, '_>, w: &mut InodeWalker)
    -> Result<(), BchError>
{
    let Some(inum) = w.cur_inum() else { return Ok(()) };
    let recalculate_sums = w.recalculate_sums();

    bch_err_fn!(t.fs(), w.inodes_mut().iter_mut()
        .try_for_each(|i| check_dir_nlink(t, inum, recalculate_sums, i)))
}

/// check_subdir_count_notnested() from inside the commit loop: its repairs
/// commit for themselves, dropping what this attempt queued - the fsck_err
/// log entries - see self_committing(); the key is retried after.
fn check_subdir_dirents_count(t: &TransAttempt<'_, '_>, w: &mut InodeWalker) -> Result<(), BchError> {
    t.self_committing(|t| check_subdir_count_notnested(t, w))
}

// ---------------------------------------------------------------------------
// DT_SUBVOL dirents

/// A subvolume whose snapshot is @snapshot or a descendant of it.
fn find_snapshot_subvol(t: &TransAttempt<'_, '_>, snapshot: u32) -> Result<Option<u32>, BchError> {
    let mut iter = BtreeIter::new(t, c::btree_id::subvolumes, POS_MIN, BtreeIterFlags::empty());

    iter.for_each_norestart(t, |_, k| Ok(
        match k.as_subvolume() {
            Some(s) if snapshot::is_ancestor(t, u32::from_le(s.snapshot), snapshot) =>
                ControlFlow::Break(Some(k.k.p.offset as u32)),
            _ => ControlFlow::Continue(()),
        }))
}

/// The subvolume @k, a DT_SUBVOL dirent, lives in: it must exist and see the
/// dirent's snapshot. Returns the subvolume, after any repair.
fn check_dirent_parent_subvol<'t>(
    t:      &TransAttempt<'_, 't>,
    iter:   &BtreeIter<'t>,
    k:      BkeySC<'_>,
    parent: u32,
) -> Result<u32, BchError> {
    let trans = t.trans();
    let fs = trans.fs();
    let d_snapshot = k.k.p.snapshot;

    let parent_snapshot = subvolume::get_snapshot_nowarn(trans, parent).found()?;

    // A reference to the root subvolume is never the broken side: subvol 1's
    // existence is an invariant, and check_root() recreates it if lost.
    // Rewiring parent_subvol around the absence reparents the root
    // directory's subvolume dirents onto arbitrary subvolumes; left alone,
    // they're valid the moment the root subvolume is back:
    if parent_snapshot.is_none() && parent == c::BCACHEFS_ROOT_SUBVOL {
        return Ok(parent);
    }

    if parent_snapshot.is_some_and(|s| snapshot::is_ancestor(trans, s, d_snapshot)) {
        return Ok(parent);
    }

    let new_parent = find_snapshot_subvol(t, d_snapshot)?;

    if new_parent.is_none() && fs.btree_lost_data(c::btree_id::subvolumes) {
        // No subvolume sees the dirent's snapshot - but we lost subvolumes,
        // so reconstruct the one it names:
        let root = check::reconstruct_subvol_root(t, d_snapshot, parent, None)?;
        check::reconstruct_subvol(t, d_snapshot, parent, root)?;
        return Ok(parent);
    }

    // The parent exists but doesn't see the dirent, and another subvolume
    // does: that's the one to move it to. Otherwise it's missing, as far as
    // the dirent is concerned:
    let repair = match (parent_snapshot, new_parent) {
        (Some(parent_snapshot), Some(_)) =>
            fsck_err!(trans, id::dirent_not_visible_in_parent_subvol,
                      "dirent not visible in parent_subvol (not an ancestor of subvol snap {parent_snapshot})\n{}",
                      k.to_text(fs))?,
        _ =>
            fsck_err!(trans, id::dirent_to_missing_parent_subvol,
                      "dirent parent_subvol points to missing subvolume\n{}", k.to_text(fs))?,
    };
    if !repair {
        return Ok(parent);
    }

    let Some(new_parent) = new_parent else {
        bch_err!(fs, "could not find a subvol for snapshot {d_snapshot}");
        return Err(fs.err(bch_errcode::BCH_ERR_fsck_repair_unimplemented).into());
    };

    // The dirent is rewritten at its own position, which may be an interior
    // snapshot node - dirents in old snapshots need this repair too:
    let mut n = t.bkey_reassemble(k)?;
    n.k_i_mut().as_mut_dirent().expect("a dirent").set_parent_subvol(new_parent);
    t.update(iter, &n, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)?;

    // The fs_path_parent check after this repairs the subvolume to agree with
    // the dirent, so it has to agree with the dirent just written, not the
    // one replaced - otherwise a single pass writes two different answers,
    // and each fsck after moves one to match the other's stale value.
    Ok(new_parent)
}

fn check_dirent_to_subvol<'t>(
    t:      &TransAttempt<'_, 't>,
    iter:   &BtreeIter<'t>,
    k:      BkeySC<'_>,
    child:  u32,
    parent: u32,
) -> Result<(), BchError> {
    let parent = check_dirent_parent_subvol(t, iter, k, parent)?;
    let trans = t.trans();
    let fs = trans.fs();

    let mut subvol_iter = BtreeIter::new(trans, c::btree_id::subvolumes, pos(0, child as u64),
                                         BtreeIterFlags::empty());
    let s = subvol_iter.peek_slot_typed(t, c::bch_bkey_type::KEY_TYPE_subvolume).found()?;

    // A deleted-state subvolume is a tombstone pending the snapshot sweep:
    // subvolume::get() reports those as ENOENT, but this is a raw read (the
    // fs_path_parent repair needs the key). Treat it as missing here too -
    // otherwise the dirent outlives the subvolume, and fsck stops converging
    // once the sweep removes the key:
    let s = s.filter(|s| {
        subvolume::val(*s).expect("typed").state() != Some(c::bch_subvolume_state::SUBVOLUME_STATE_deleted)
    });

    let Some(s) = s else {
        if inode_fsck_err!(trans, k.k.p, id::dirent_to_missing_subvol,
                           "dirent points to missing subvolume\n{}", k.to_text(fs))? {
            dirent::fsck_remove(t, k.k.p)?;
        }
        return Ok(());
    };

    let v = subvolume::val(s).expect("typed");
    let fs_path_parent  = u32::from_le(v.fs_path_parent);
    let target_inum     = u64::from_le(v.inode);
    let target_snapshot = u32::from_le(v.snapshot);

    if fs_path_parent != parent {
        let mut path = Printbuf::new();
        namei::inum_to_path(trans, c::subvol_inum { subvol: child as u64, inum: target_inum }, &mut path)?;

        if fsck_err!(trans, id::subvol_fs_path_parent_wrong,
                     "subvol with wrong fs_path_parent, should be {parent}\n{path}\n{}",
                     s.to_text(fs))? {
            let mut n = t.bkey_reassemble(s)?;
            n.k_i_mut().as_mut_subvolume().expect("a subvolume").fs_path_parent = parent.to_le();
            t.update(&subvol_iter, &n, UpdateTriggerFlags::empty())?;
        }
    }

    let Some(mut subvol_root) = inode::find_by_inum_snapshot(trans, target_inum, target_snapshot,
                                                             BtreeIterFlags::empty()).found()? else {
        bch_err!(fs, "subvol {child} points to missing inode root {target_inum}");
        return Err(fs.err(bch_errcode::BCH_ERR_fsck_repair_unimplemented).into());
    };

    if fsck_err_on!(trans, parent != subvol_root.bi_parent_subvol, id::inode_bi_parent_wrong,
                    "subvol root {target_inum} has wrong bi_parent_subvol: got {}, should be {parent}",
                    subvol_root.bi_parent_subvol)? {
        subvol_root.bi_parent_subvol = parent;
        subvol_root.bi_snapshot      = target_snapshot;
        inode::fsck_write(t, &mut subvol_root)?;
    }

    namei::check_dirent_target(trans, iter, k, &mut subvol_root)
}

// ---------------------------------------------------------------------------
// Dirents to inodes

/// The dirent's target inode is missing, but @inum has extents, dirents or
/// xattrs in @snapshot: offer to recreate it - as what its keys say it was,
/// or, for xattrs, which don't say, what the dirent does (@d_type). Returns
/// whether the repair was declined - the dirent is then left alone, rather
/// than removed as pointing at nothing.
///
/// Xattrs too, or a file whose only contents are xattrs loses its name: the
/// dirent goes as pointing at nothing, and check_xattrs reconstructs the
/// inode after, for check_unreachable_inodes to file under lost+found.
fn maybe_reconstruct_inum(
    t:        &TransAttempt<'_, '_>,
    inum:     u64,
    snapshot: u32,
    d_type:   u8,
) -> Result<bool, BchError> {
    let trans = t.trans();

    for (btree, type_) in [(c::btree_id::extents, "reg"),
                           (c::btree_id::dirents, "dir"),
                           (c::btree_id::xattrs,  "with xattrs")] {
        let has_contents = {
            let mut iter = BtreeIter::new(trans, btree, spos(inum, 0, snapshot), BtreeIterFlags::empty());
            iter.peek_max(t, pos(inum, u64::MAX))?.is_some()
        };
        if !has_contents {
            continue;
        }

        if !inode_fsck_err!(trans, spos(0, inum, snapshot), id::missing_inode_with_contents,
                            "inode {inum}:{snapshot} type {type_} missing, but contents found: reconstruct?")? {
            return Ok(true);
        }

        check::reconstruct_inode(t, btree, snapshot, inum, Some(d_type))?;
        t.commit(None, CommitFlags::NO_ENOSPC)?;
        return Err(t.restart(bch_errcode::BCH_ERR_transaction_restart_commit));
    }

    Ok(false)
}

/// The dirent's target exists only in snapshots descending from the dirent's:
/// subvolumes in a sibling branch see the dirent and not the inode. Deleting
/// the dirent would orphan the inodes, so copy it down to each snapshot the
/// inode exists in, and delete the original.
fn move_dirent_to_inode_snapshots(
    t:  &TransAttempt<'_, '_>,
    k:  BkeySC<'_>,
    st: &CheckDirents,
) -> Result<(), BchError> {
    let trans = t.trans();

    for i in st.target.inodes() {
        t.commit_lazy_if_full(CommitFlags::NO_ENOSPC)?;

        let snapshot = i.inode.bi_snapshot;

        // The inode-side state firing this repair is unchanged by the copies,
        // so a re-drive after a partially committed batch fires it again: skip
        // the copies already committed, or the commit_lazy_if_full() restart
        // can't make forward progress:
        let copied = {
            let mut probe = BtreeIter::new(trans, c::btree_id::dirents,
                                           spos(k.k.p.inode, k.k.p.offset, snapshot),
                                           BtreeIterFlags::empty());
            let old = probe.peek_slot(t)?.expect("a slot always has a key");
            old.k.p.snapshot == snapshot &&
                old.key_type() == k.key_type() &&
                old.val_bytes() == k.val_bytes()
        };
        if copied {
            continue;
        }

        let mut n = t.bkey_make_mut_noupdate(k)?;
        n.k_mut().p.snapshot = snapshot;
        t.insert(c::btree_id::dirents, n, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)?;
    }

    // Delete with the hash info we already have - dirent::fsck_remove()
    // rederives it from the first inode version it finds:
    let mut del_iter = BtreeIter::new(trans, c::btree_id::dirents, k.k.p, BtreeIterFlags::INTENT);
    t.iter_traverse(&mut del_iter)?;
    str_hash::delete_at::<Dirents>(t, &st.hash_info, &mut del_iter,
                                   UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)
}

/// Returns whether the rest of the key is to be skipped.
fn check_dirent_to_inode<'t>(
    t:    &TransAttempt<'_, 't>,
    iter: &BtreeIter<'t>,
    k:    BkeySC<'_>,
    inum: u64,
    st:   &mut CheckDirents,
) -> Result<bool, BchError> {
    let trans = t.trans();
    let fs = trans.fs();
    let d_snapshot = k.k.p.snapshot;

    st.target.get_visible(t, &mut st.s, inum)?;

    if st.target.inodes().is_empty() {
        let d_type = Dirent::new(k).expect("a dirent").d_type();
        if maybe_reconstruct_inum(t, inum, d_snapshot, d_type)? {
            return Ok(true);
        }
    }

    // The inode must exist in an ancestor snapshot of the dirent: that's what
    // makes the dirent resolvable from every subvolume leaf that can see it.
    // get_visible() also finds inodes in descendant snapshots - reachable
    // from *some* leaf, but a subvolume in a sibling branch sees the dirent
    // and not the inode, and lookups there return ENOENT. Versions come
    // newest snapshot first, from the dirent's down, so an ancestor inode, if
    // there is one, is first:
    if let Some(first) = st.target.inodes().first() {
        if !snapshot::is_ancestor(trans, d_snapshot, first.inode.bi_snapshot) &&
           inode_fsck_err!(trans, k.k.p, id::dirent_to_inode_in_descendant_snapshot,
                           "dirent with inode(s) only in descendant snapshots:\n{}", k.to_text(fs))? {
            move_dirent_to_inode_snapshots(t, k, st)?;
            // The copies are ahead of the walk: a second pass checks them,
            // and counts them in their directories.
            st.need_second_pass = true;
            return Ok(true);
        }
    }

    if st.target.inodes().is_empty() &&
       inode_fsck_err!(trans, k.k.p, id::dirent_to_missing_inode,
                       "dirent points to missing inode:\n{}", k.to_text(fs))? {
        dirent::fsck_remove(t, k.k.p)?;
    }

    for i in st.target.inodes_mut() {
        // One repair per snapshot version of the target inode, bounded only
        // by snapshot count. The re-drive converges: get_visible() rereads
        // the versions, and committed repairs no longer fire.
        t.commit_lazy_if_full(CommitFlags::NO_ENOSPC)?;
        check::own_version(trans, &mut i.inode, d_snapshot);
        namei::check_dirent_target(trans, iter, k, &mut i.inode)?;
    }

    for &snapshot in st.target.deletes() {
        if st.s.has_id(snapshot) {
            continue;
        }

        t.commit_lazy_if_full(CommitFlags::NO_ENOSPC)?;

        let mut delete_iter = BtreeIter::new(trans, c::btree_id::dirents,
                                             spos(k.k.p.inode, k.k.p.offset, snapshot),
                                             BtreeIterFlags::INTENT);

        // The deletes list is derived from inode-side state the whiteouts
        // don't change: check that the dirent is still visible in this
        // snapshot, both so a re-drive after a partially committed batch
        // skips the committed whiteouts (or the commit_lazy_if_full() restart
        // can't make forward progress), and so an already invisible dirent
        // isn't reported and "repaired" redundantly:
        let visible = delete_iter.peek_slot(t)?.expect("a slot always has a key");
        if visible.key_type() != c::bch_bkey_type::KEY_TYPE_dirent {
            continue;
        }
        let visible_pos = visible.k.p;

        if inode_fsck_err!(trans, visible_pos, id::dirent_to_overwritten_inode,
                           "dirent points to inode overwritten in snapshot {snapshot}:\n{}",
                           k.to_text(fs))? {
            str_hash::delete_at::<Dirents>(t, &st.hash_info, &mut delete_iter,
                                           UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)?;
        }
    }

    Ok(false)
}

// ---------------------------------------------------------------------------

fn check_dirent<'t>(
    t:    &TransAttempt<'_, 't>,
    iter: &BtreeIter<'t>,
    k:    BkeySC<'_>,
    st:   &mut CheckDirents,
) -> Result<(), BchError> {
    let trans = t.trans();
    let fs = trans.fs();

    if snapshot::check_key_has_snapshot(trans, iter, k)? {
        return Ok(());
    }

    st.s.update(k.k.p)?;

    if k.key_type() == c::bch_bkey_type::KEY_TYPE_whiteout {
        return Ok(());
    }

    // The walk has left the last directory: its subdirectory counts are done.
    if st.dir.cur_inum().is_some_and(|inum| inum != k.k.p.inode) {
        check_subdir_dirents_count(t, &mut st.dir)?;
    }

    let Some(w) = st.dir.walk(t, iter, k)? else { return Ok(()) };
    if w.first_this_inode {
        st.hash_info = str_hash::hash_info_init(fs, w.inode)?;
    }
    st.hash_info.cf_encoding = inode::cf_encoding(fs, w.inode);

    match str_hash::check_key::<Dirents>(trans, Some(&mut st.s), &mut st.hash_info,
                                         k, &mut st.need_second_pass) {
        Ok(()) => {}
        // A casefold mismatch repair deletes the dirent and recreates it
        // where it hashes to: nothing more to check here.
        Err(e) if e.matches(bch_errcode::BCH_ERR_str_hash_key_repaired) => return Ok(()),
        Err(e) => {
            // A hash info repair leaves the walker's inodes - and hash_info,
            // initialized from them - stale:
            st.dir.invalidate();
            return Err(e);
        }
    }

    let Some(d) = Dirent::new(k) else { return Ok(()) };
    let d_snapshot = k.k.p.snapshot;
    // Key values aren't valid after a commit without revalidating:
    let have_dir = d.d_type() as u32 == c::DT_DIR;

    match d.target() {
        DirentTarget::Subvol { child, parent } => check_dirent_to_subvol(t, iter, k, child, parent)?,
        DirentTarget::Inode(inum) => {
            if check_dirent_to_inode(t, iter, k, inum, st)? {
                return Ok(());
            }
        }
    }

    t.commit(None, CommitFlags::NO_ENOSPC)?;

    if have_dir {
        for i in st.dir.visible_mut(trans, &mut st.s, d_snapshot) {
            i.count += 1;
        }
    }

    Ok(())
}

fn check_dirents(fs: &Fs) -> Result<(), BchError> {
    let trans = crate::btree_trans!(fs);
    let mut st = CheckDirents {
        s:                SnapshotsSeen::new(),
        dir:              InodeWalker::new(),
        target:           InodeWalker::new(),
        hash_info:        Default::default(),
        need_second_pass: false,
    };
    let mut did_second_pass = false;

    loop {
        let progress = Progress::recovery(fs, c"bch2_check_dirents", &[c::btree_id::dirents], &[]);
        let mut iter = BtreeIter::new(&trans, c::btree_id::dirents,
                                      pos(c::BCACHEFS_ROOT_INO as u64, 0),
                                      BtreeIterFlags::PREFETCH | BtreeIterFlags::ALL_SNAPSHOTS);

        iter.for_each_commit(&trans, None, CommitFlags::NO_ENOSPC, |t, iter, k| {
            progress.update(t, iter)?;
            check_dirent(t, iter, k, &mut st)
        })?;

        // The last directory's subdirectory counts, as in the loop, with the
        // restart it may end in retried here.
        lockrestart_do(&trans, |t| check_subdir_dirents_count(t, &mut st.dir))?;

        // A directory that's become a file, or the reverse, may have been
        // counted in its parent before the walk got to it:
        st.need_second_pass |= st.dir.take_changed_inode_type();

        if !st.need_second_pass {
            return Ok(());
        }

        if did_second_pass {
            bch_err!(fs, "dirents not repairing");
            return Err(c::EINVAL.into());
        }

        bch_info!(fs, "check_dirents requires second pass");
        did_second_pass = true;
        st.need_second_pass = false;
    }
}

crate::recovery_pass!(bch2_check_dirents => check_dirents);
