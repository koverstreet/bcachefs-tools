// SPDX-License-Identifier: GPL-2.0

//! check_unreachable_inodes: reattach inodes that aren't unlinked but have no
//! dirent. It runs after check_inodes and check_dirents, so backpointers are
//! valid and set wherever a dirent exists: an inode without one is unreachable.
//!
//! An unreachable inode is reattached at its oldest version that needs it: by
//! propagating up the dirent of a descendant version that's still attached, if
//! there is one, else into lost+found.
//!
//! XXX: racy w.r.t. hardlink removal in online fsck.

use crate::btree::bkey::{spos, POS_MIN};
use crate::btree::iter::{
    BtreeIter, BtreeIterFlags, CommitFlags, TransAttempt, TransBkey,
    UpdateTriggerFlags,
};
use crate::c;
use crate::lostfound::{self, inode_should_reattach};
use crate::errcode::{BchError, Found};
use crate::fs::Fs;
use crate::init::error::id;
use crate::init::progress::Progress;
use crate::snapshots::snapshot;
use crate::{bch_err, inode_fsck_err};
use crate::dirent::Dirent;
use crate::{inode, namei};
use core::ops::ControlFlow;

/// Whether this inum is a subvolume root, from the first version seen.
///
/// bi_subvol can't be read off an arbitrary version: taking a snapshot updates
/// the new subvolume's root inode but not the old one's, and older versions may
/// predate the subvolume, so an old version of a root reads bi_subvol == 0.
///
/// The first version seen is one fsck maintains bi_subvol on: within an inum we
/// visit snapshot IDs ascending, a snapshot's ID is less than its parent's
/// (bch2_snapshot_validate()), and only a descendant's version can shadow a
/// version - so nothing shadows it. Being a subvolume root is a property of the
/// inum, so the answer holds for every version.
#[derive(Default)]
struct SubvolRootSeen {
    inum:           u64,
    is_subvol_root: bool,
}

/// We find versions leaves first: walk @inode up to the oldest version that
/// also needs reattaching.
fn find_oldest_inode_needs_reattach(
    t:     &TransAttempt<'_, '_>,
    inode: &mut c::bch_inode_unpacked,
) -> Result<(), BchError> {
    // U32_MAX is a root: nothing is older
    let Some(start) = inode.bi_snapshot.checked_add(1) else { return Ok(()) };

    let fs = t.fs();
    let inum = inode.bi_inum;
    let mut iter = BtreeIter::new(t, c::btree_id::inodes, spos(0, inum, start),
                                  BtreeIterFlags::ALL_SNAPSHOTS);

    iter.for_each_max_norestart(t, spos(0, inum, u32::MAX), |_, k| {
        if !snapshot::is_ancestor(t, inode.bi_snapshot, k.k.p.snapshot) {
            return Ok(ControlFlow::Continue(()));
        }

        if !inode::bkey_is_inode(k.k) {
            return Ok(ControlFlow::Break(()));
        }

        let parent = inode::unpack(fs, k);
        if !inode_should_reattach(&parent) {
            return Ok(ControlFlow::Break(()));
        }

        *inode = parent;
        Ok(ControlFlow::Continue(()))
    })
}

/// Incomplete snapshot deletion can leave a dirent further down the snapshot
/// tree than its inode: ancestor versions orphaned, a descendant still
/// attached. Find the descendant's dirent, to propagate up here rather than
/// reattach in lost+found. It must name this inode, the directory must be
/// visible and not unlinked here, and the slot empty: a whiteout means the
/// entry was deliberately deleted in this view.
///
/// A copy: the caller holds it across fsck_err(), which can drop locks.
fn find_attached_dirent_in_descendant<'a, 't>(
    t:     &TransAttempt<'a, 't>,
    inode: &c::bch_inode_unpacked,
) -> Result<Option<TransBkey<'a, 't>>, BchError> {
    // A subvolume root's dirent is in the parent subvolume's snapshots
    if inode.bi_subvol != 0 {
        return Ok(None);
    }

    let Some(child) = attached_descendant_version(t, inode)? else { return Ok(None) };

    let mut snapshot = child.bi_snapshot;
    let mut dirent_iter = BtreeIter::uninit();
    let Some(d) = namei::inode_get_dirent(t, &mut dirent_iter, &child, &mut snapshot).found()? else {
        return Ok(None);
    };

    if !namei::dirent_points_to_inode(Dirent::new(d).expect("a dirent"), inode) {
        return Ok(None);
    }

    // Visible at the descendant, so on its path to the root, as our snapshot
    // is: the two are comparable
    let dst = spos(d.k.p.inode, d.k.p.offset, inode.bi_snapshot);

    match dst_slot(t, dst, inode, BtreeIterFlags::empty())? {
        DstSlot::Ours  => {}
        DstSlot::Taken => return Ok(None),
        // No hash check: check_inodes keeps a directory's hash info the same
        // across versions
        DstSlot::Empty => {
            let dir = inode::find_by_inum_snapshot(t, d.k.p.inode, inode.bi_snapshot,
                                                   BtreeIterFlags::empty()).found()?;
            if dir.is_none_or(|dir| dir.flag(c::bch_inode_flags::BCH_INODE_unlinked)) {
                return Ok(None);
            }
        }
    }

    Ok(Some(t.bkey_make_mut_noupdate(d)?))
}

/// The first attached version of @inode in a strict descendant snapshot.
fn attached_descendant_version(
    t:     &TransAttempt<'_, '_>,
    inode: &c::bch_inode_unpacked,
) -> Result<Option<c::bch_inode_unpacked>, BchError> {
    // Descendants have smaller IDs (and 0 is never one)
    let Some(end) = inode.bi_snapshot.checked_sub(1) else { return Ok(None) };

    let fs = t.fs();
    let inum = inode.bi_inum;
    let mut child = None;
    let mut iter = BtreeIter::new(t, c::btree_id::inodes, spos(0, inum, 0),
                                  BtreeIterFlags::ALL_SNAPSHOTS);

    iter.for_each_max_norestart(t, spos(0, inum, end), |_, k| {
        if !inode::bkey_is_inode(k.k) ||
           !snapshot::is_ancestor(t, k.k.p.snapshot, inode.bi_snapshot) {
            return Ok(ControlFlow::Continue(()));
        }

        let u = inode::unpack(fs, k);
        if u.has_backpointer() && u.bi_parent_subvol == 0 {
            child = Some(u);
            return Ok(ControlFlow::Break(()));
        }
        Ok(ControlFlow::Continue(()))
    })?;

    Ok(child)
}

/// Where a propagated dirent would go, as our snapshot sees it.
enum DstSlot {
    /// Propagated already, from an older version earlier in the pass: only the
    /// backpointer needs fixing; lost+found would make a second link.
    Ours,
    Empty,
    /// Another inode's dirent - inserting would hide it from every view below -
    /// or a whiteout.
    Taken,
}

fn dst_slot(
    t:     &TransAttempt<'_, '_>,
    pos:   c::bpos,
    inode: &c::bch_inode_unpacked,
    flags: BtreeIterFlags,
) -> Result<DstSlot, BchError> {
    let mut vis_iter = BtreeIter::new(t, c::btree_id::dirents, pos, flags);
    let vis = vis_iter.peek_slot(t)?.expect("a slot always has a key");

    if let Some(vis) = Dirent::new(vis) {
        return Ok(if namei::dirent_points_to_inode(vis, inode) { DstSlot::Ours } else { DstSlot::Taken });
    }

    let mut dst_iter = BtreeIter::new(t, c::btree_id::dirents, pos,
                                      flags | BtreeIterFlags::ALL_SNAPSHOTS);
    Ok(if dst_iter.peek_slot(t)?.expect("a slot always has a key").k.is_deleted() {
        DstSlot::Empty
    } else {
        DstSlot::Taken
    })
}

/// Copy @new, a descendant version's dirent, to @inode's snapshot - unless
/// it's there already - and point the backpointer at it.
fn reattach_via_descendant_dirent<'a, 't>(
    t:       &TransAttempt<'a, 't>,
    inode:   &mut c::bch_inode_unpacked,
    mut new: TransBkey<'a, 't>,
) -> Result<(), BchError> {
    new.k_mut().p.snapshot = inode.bi_snapshot;
    let pos = new.k().p;

    // Again, under the intent lock: fsck_err() may have dropped locks, and
    // this pass can run online
    match dst_slot(t, pos, inode, BtreeIterFlags::INTENT)? {
        DstSlot::Ours  => {}
        DstSlot::Empty => {
            let mut iter = BtreeIter::new(t, c::btree_id::dirents, pos,
                                          BtreeIterFlags::ALL_SNAPSHOTS | BtreeIterFlags::INTENT);
            t.iter_traverse(&mut iter)?;
            t.update(&iter, &new, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)?;
        }
        DstSlot::Taken => {
            bch_err!(t.fs(),
                     "not propagating dirent for inode {}:{}: destination {pos} now occupied",
                     inode.bi_inum, inode.bi_snapshot);
            return Ok(());
        }
    }

    inode.bi_dir        = pos.inode;
    inode.bi_dir_offset = pos.offset;
    inode::fsck_write(t, inode)
}

fn check_unreachable_inode(
    t:    &TransAttempt<'_, '_>,
    k:    crate::btree::bkey::BkeySC<'_>,
    seen: &mut SubvolRootSeen,
) -> Result<(), BchError> {
    if !inode::bkey_is_inode(k.k) {
        return Ok(());
    }

    let mut u = inode::unpack(t.fs(), k);

    // Every version, including those that don't need reattaching
    if u.bi_inum != seen.inum {
        seen.inum           = u.bi_inum;
        seen.is_subvol_root = u.bi_subvol != 0;
    }

    if !inode_should_reattach(&u) {
        return Ok(());
    }

    // Not for a subvolume root: its one dirent is in the parent subvolume and
    // isn't versioned, so there are no ancestor versions to climb. Climbing
    // anyway reattached an ancestor - without bi_subvol - in lost+found, and
    // the root was then reachable twice: inode_dir_multiple_links, emergency
    // read-only (field report, 2026-08-04).
    if !seen.is_subvol_root {
        find_oldest_inode_needs_reattach(t, &mut u)?;
    }

    let pos = spos(0, u.bi_inum, u.bi_snapshot);

    // Attached in a descendant: propagate that, rather than a lost+found
    // entry visible in every view below
    if let Some(d) = find_attached_dirent_in_descendant(t, &u)? {
        return if inode_fsck_err!(t, pos, id::inode_unreachable_dirent_in_descendant,
                                  "unreachable inode with dirent in descendant snapshot {}, propagating:\n{u}",
                                  d.k().p.snapshot)? {
            reattach_via_descendant_dirent(t, &mut u, d)
        } else {
            Ok(())
        };
    }

    if inode_fsck_err!(t, pos, id::inode_unreachable, "unreachable inode:\n{u}")? {
        return lostfound::reattach_inode(t, &mut u);
    }

    Ok(())
}

fn check_unreachable_inodes(fs: &Fs) -> Result<(), BchError> {
    let trans = crate::btree_trans!(fs);
    let progress = Progress::recovery(fs, c"bch2_check_unreachable_inodes",
                                      &[c::btree_id::inodes], &[]);
    let mut seen = SubvolRootSeen::default();

    let mut iter = BtreeIter::new(&trans, c::btree_id::inodes, POS_MIN,
                                  BtreeIterFlags::PREFETCH | BtreeIterFlags::ALL_SNAPSHOTS);

    iter.for_each_commit(&trans, None, CommitFlags::NO_ENOSPC, |t, iter, k| {
        progress.update(t, iter)?;
        check_unreachable_inode(t, k, &mut seen)
    })
}

crate::recovery_pass!(bch2_check_unreachable_inodes => check_unreachable_inodes);
