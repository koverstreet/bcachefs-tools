// SPDX-License-Identifier: GPL-2.0

//! fsck: the check_unreachable_inodes recovery pass - reattach unreachable
//! (but not unlinked) inodes.
//!
//! Runs after check_inodes and check_dirents, so we know that inode
//! backpointer fields point to valid dirents, and every inode that has a
//! dirent pointing to it has its backpointer set - so we're just looking for
//! non-unlinked inodes without backpointers.
//!
//! An unreachable version is reattached at the oldest version that needs it,
//! and preferably where a descendant version is still attached - by
//! propagating that dirent up - rather than in lost+found.
//!
//! Changes from the C: find_oldest_inode_needs_reattach() no longer wraps
//! the start of its walk past U32_MAX - the C's bi_snapshot + 1 walked every
//! version from snapshot 0, and only found nothing older by luck (no snapshot
//! is an ancestor of U32_MAX but itself).
//!
//! XXX: racy w.r.t. hardlink removal in online fsck.

use crate::btree::bkey::{spos, POS_MIN};
use crate::btree::iter::{
    BtreeIter, BtreeIterFlags, BtreeTrans, CommitFlags, TransAttempt, TransBkey, TransRet,
    UpdateTriggerFlags,
};
use crate::c;
use crate::c::bch_inode_flags::BCH_INODE_unlinked;
use crate::check::{self, inode_should_reattach};
use crate::errcode::{BchError, Found};
use crate::fs::Fs;
use crate::init::error::id;
use crate::init::progress::Progress;
use crate::snapshots::snapshot;
use crate::{bch_err, inode_fsck_err};
use crate::{dirent, inode};
use core::ops::ControlFlow;

/// Is this inode number a subvolume root? Answered once per inum, from the
/// first version we see.
///
/// bi_subvol cannot be read off an arbitrary version. Taking a snapshot
/// updates the root inode of the new subvolume but not of the old, so the
/// version left behind at the now-interior node is still the live root of
/// the old subvolume and was never rewritten; versions older still may
/// predate the subvolume entirely. An old version of a subvolume root
/// legitimately reads bi_subvol == 0, and trusting that is how we ended up
/// reattaching one into lost+found.
///
/// The first version we see for an inum is different, and one bool taken from
/// it then carries to the rest:
///
///  1. We iterate BTREE_ID_inodes with all_snapshots from POS_MIN, and inode
///     keys sort by (inum, snapshot) - so within an inum we visit snapshot IDs
///     in ascending order.
///  2. A snapshot's ID is always strictly less than its parent's; the
///     snapshot key validator enforces it (snapshot_parent_bad,
///     bch2_snapshot_validate()). So every descendant of a node sorts before
///     that node.
///  3. Version B shadows version A only if B lives at a descendant of A's
///     snapshot. By (2) B sorts before A, so by (1) we would already have
///     seen B when we reach A.
///  4. Hence nothing shadows the first version we see for an inum: some live
///     view resolves to it. That is the version fsck maintains bi_subvol on -
///     check_subvols() ran before us and repairs it there, and check_inode()
///     only validates bi_subvol where it is meaningful.
///  5. Whether an inum is a subvolume root is a property of the number, not
///     of any one version, so the answer is good for all of them.
///
/// Only the boolean is carried, not the subvolume ID: the first version we
/// land on may belong to any of the subvolumes rooted at this inum, and which
/// one it is says nothing.
#[derive(Default)]
struct SubvolRootSeen {
    inum:           u64,
    is_subvol_root: bool,
}

/// We look for inodes to reattach in natural key order, leaves first, but we
/// should do the reattach at the oldest version that needs to be reattached:
/// walk @inode up to it.
fn find_oldest_inode_needs_reattach(
    trans: &BtreeTrans<'_>,
    inode: &mut c::bch_inode_unpacked,
) -> Result<(), BchError> {
    // U32_MAX is a root: nothing is older
    let Some(start) = inode.bi_snapshot.checked_add(1) else { return Ok(()) };

    let fs = trans.fs();
    let inum = inode.bi_inum;
    let mut iter = BtreeIter::new(trans, c::btree_id::inodes, spos(0, inum, start),
                                  BtreeIterFlags::ALL_SNAPSHOTS);

    iter.for_each_max_norestart(spos(0, inum, u32::MAX), |_, k| {
        if !snapshot::is_ancestor(trans, inode.bi_snapshot, k.k.p.snapshot) {
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

/// An unreachable inode version may still be attached in a descendant
/// snapshot: incomplete snapshot deletion can move a dirent further down the
/// snapshot tree than the inode that points to it (an interrupted pass
/// resumes against new topology, so the two stop at different termini),
/// leaving ancestor views orphaned while the descendant view is intact.
///
/// check_inodes zeroed this version's backpointer, but the attached
/// descendant version still carries its verified one - find that dirent, so
/// we can propagate a copy up to this version's snapshot instead of
/// reattaching in lost+found.
///
/// Requirements checked here: the dirent names this inode from a strict
/// descendant, the parent directory is visible (and not unlinked) in this
/// version's view, and the destination slot is empty - a whiteout there means
/// the entry was deliberately deleted in this view, and must not be
/// resurrected.
///
/// Returns a copy in transaction memory: the caller uses it across fsck_err(),
/// which can cycle transaction locks, so a reference into a btree node buffer
/// would be a use after unlock.
fn find_attached_dirent_in_descendant<'a, 't>(
    t:     &TransAttempt<'a, 't>,
    inode: &c::bch_inode_unpacked,
) -> Result<Option<TransBkey<'a, 't>>, BchError> {
    let trans = t.trans();

    // Dirents pointing to subvolume roots live in the parent subvolume - a
    // different snapshot space; those take the reattach path:
    if inode.bi_subvol != 0 {
        return Ok(None);
    }

    let Some(child) = attached_descendant_version(trans, inode)? else { return Ok(None) };

    let mut snapshot = child.bi_snapshot;
    let mut dirent_iter = BtreeIter::uninit();
    let Some(d) = inode::get_dirent(trans, &mut dirent_iter, &child, &mut snapshot).found()? else {
        return Ok(None);
    };

    if !dirent::points_to_inode(d.as_dirent().expect("a dirent"), inode) {
        return Ok(None);
    }

    // (d is visible at the descendant, so it lies on the descendant's
    // rootward path and is always comparable with our snapshot - no separate
    // ancestry check is needed.)
    let dst = spos(d.k.p.inode, d.k.p.offset, inode.bi_snapshot);

    match dst_slot(trans, dst, inode, BtreeIterFlags::empty())? {
        DstSlot::Ours  => {}
        DstSlot::Taken => return Ok(None),
        // An insert also requires the parent directory to be visible and not
        // unlinked. (No hash check needed: check_dirents verified the dirent
        // at the descendant's view, and hash info is invariant across an
        // inode's snapshot versions - enforced by check_inodes - so it hashes
        // identically under the directory here.)
        DstSlot::Empty => {
            let dir = inode::find_by_inum_snapshot(trans, d.k.p.inode, inode.bi_snapshot,
                                                   BtreeIterFlags::empty()).found()?;
            if !dir.is_some_and(|dir| !dir.flag(BCH_INODE_unlinked)) {
                return Ok(None);
            }
        }
    }

    Ok(Some(t.bkey_make_mut_noupdate(d)?))
}

/// A version of @inode's inum in a strict descendant of its snapshot that is
/// attached - has a backpointer, in its own subvolume: the first in key
/// order, leaves first.
fn attached_descendant_version(
    trans: &BtreeTrans<'_>,
    inode: &c::bch_inode_unpacked,
) -> Result<Option<c::bch_inode_unpacked>, BchError> {
    // Descendants have smaller IDs (and 0 is never one)
    let Some(end) = inode.bi_snapshot.checked_sub(1) else { return Ok(None) };

    let fs = trans.fs();
    let inum = inode.bi_inum;
    let mut child = None;
    let mut iter = BtreeIter::new(trans, c::btree_id::inodes, spos(0, inum, 0),
                                  BtreeIterFlags::ALL_SNAPSHOTS);

    iter.for_each_max_norestart(spos(0, inum, end), |_, k| {
        if !inode::bkey_is_inode(k.k) ||
           !snapshot::is_ancestor(trans, k.k.p.snapshot, inode.bi_snapshot) {
            return Ok(ControlFlow::Continue(()));
        }

        let u = inode::unpack(fs, k);
        if inode::has_backpointer(&u) && u.bi_parent_subvol == 0 {
            child = Some(u);
            return Ok(ControlFlow::Break(()));
        }
        Ok(ControlFlow::Continue(()))
    })?;

    Ok(child)
}

/// Our snapshot's view of the position a propagated dirent would go.
enum DstSlot {
    /// A dirent naming @inode is already visible - a propagation done at an
    /// older version of this inode earlier in the pass: only the backpointer
    /// needs fixing. There's no insert, and falling back to lost+found would
    /// create a duplicate link to a reachable file.
    Ours,
    /// Nothing visible, and no whiteout: a copy can go here.
    Empty,
    /// A different inode's dirent is visible - the name belongs to someone
    /// else in this view, and inserting over it would hide that file from
    /// every view below - or a whiteout: the entry was deliberately deleted
    /// here. Either way, fall back to lost+found.
    Taken,
}

fn dst_slot(
    trans: &BtreeTrans<'_>,
    pos:   c::bpos,
    inode: &c::bch_inode_unpacked,
    flags: BtreeIterFlags,
) -> Result<DstSlot, BchError> {
    let mut vis_iter = BtreeIter::new(trans, c::btree_id::dirents, pos, flags);
    let vis = vis_iter.peek_slot()?.expect("a slot always has a key");

    if let Some(vis) = vis.as_dirent() {
        return Ok(if dirent::points_to_inode(vis, inode) { DstSlot::Ours } else { DstSlot::Taken });
    }

    let mut dst_iter = BtreeIter::new(trans, c::btree_id::dirents, pos,
                                      flags | BtreeIterFlags::ALL_SNAPSHOTS);
    Ok(if dst_iter.peek_slot()?.expect("a slot always has a key").is_deleted() {
        DstSlot::Empty
    } else {
        DstSlot::Taken
    })
}

/// Attach @inode by @new, a copy of the dirent naming a descendant version,
/// moved to @inode's snapshot - or, if a matching dirent is already visible
/// there, just point the backpointer at it.
fn reattach_via_descendant_dirent<'a, 't>(
    t:       TransAttempt<'a, 't>,
    inode:   &mut c::bch_inode_unpacked,
    mut new: TransBkey<'a, 't>,
) -> TransRet<'a, 't> {
    let trans = t.trans();
    let mut t = t;

    new.k_mut().p.snapshot = inode.bi_snapshot;
    let pos = new.k().p;

    // Re-classify under the intent lock: the probe ran before fsck_err(),
    // which can cycle transaction locks, and this pass can run online.
    match dst_slot(trans, pos, inode, BtreeIterFlags::INTENT)? {
        DstSlot::Ours  => {}
        DstSlot::Empty => {
            let mut iter = BtreeIter::new(trans, c::btree_id::dirents, pos,
                                          BtreeIterFlags::ALL_SNAPSHOTS | BtreeIterFlags::INTENT);
            t = t.iter_traverse(&mut iter)?
                 .update(&mut iter, new, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)?;
        }
        DstSlot::Taken => {
            bch_err!(trans.fs(),
                     "not propagating dirent for inode {}:{}: destination {pos} now occupied",
                     inode.bi_inum, inode.bi_snapshot);
            return Ok(t);
        }
    }

    inode.bi_dir        = pos.inode;
    inode.bi_dir_offset = pos.offset;
    inode::fsck_write(t, inode)
}

fn check_unreachable_inode<'a, 't>(
    t:    TransAttempt<'a, 't>,
    k:    crate::btree::bkey::BkeySC<'_>,
    seen: &mut SubvolRootSeen,
) -> TransRet<'a, 't> {
    let trans = t.trans();

    if !inode::bkey_is_inode(k.k) {
        return Ok(t);
    }

    let mut u = inode::unpack(trans.fs(), k);

    // Before the early return below: every version has to advance this.
    if u.bi_inum != seen.inum {
        seen.inum           = u.bi_inum;
        seen.is_subvol_root = u.bi_subvol != 0;
    }

    if !inode_should_reattach(&u) {
        return Ok(t);
    }

    // Not for a subvolume root. A subvolume root has exactly one dirent, in
    // the parent subvolume, and dirents to subvolumes aren't versioned - so
    // there is no chain of unreachable ancestor versions to walk back to, and
    // the version we were handed is the one to reattach.
    //
    // Note that leaf-ness can't stand in for this: taking a snapshot updates
    // the root inode of the new subvolume, but not of the old, so a live
    // subvolume's root inode key stays at a snapshot that has since become
    // interior.
    //
    // Climbing anyway picks some ancestor version, reattaches that - and
    // because the ancestor doesn't carry bi_subvol, it gets filed into
    // lost+found as a plain directory named after its inode number, whose
    // backpointer is then propagated back down over the live versions below
    // it. The subvolume root ends up reachable both by its own DT_SUBVOL
    // dirent and by the manufactured one, which is inode_dir_multiple_links
    // -> emergency read-only at runtime. (field report, 2026-08-04)
    if !seen.is_subvol_root {
        find_oldest_inode_needs_reattach(trans, &mut u)?;
    }

    let pos = spos(0, u.bi_inum, u.bi_snapshot);

    // Attached in a descendant snapshot? Then this version has a proper home;
    // propagate the dirent up to our snapshot rather than manufacturing a
    // lost+found entry visible in every view below:
    if let Some(d) = find_attached_dirent_in_descendant(&t, &u)? {
        return if inode_fsck_err!(trans, pos, id::inode_unreachable_dirent_in_descendant,
                                  "unreachable inode with dirent in descendant snapshot {}, propagating:\n{u}",
                                  d.k().p.snapshot)? {
            reattach_via_descendant_dirent(t, &mut u, d)
        } else {
            Ok(t)
        };
    }

    if inode_fsck_err!(trans, pos, id::inode_unreachable, "unreachable inode:\n{u}")? {
        check::reattach_inode(trans, &mut u)?;
    }

    Ok(t)
}

fn check_unreachable_inodes(fs: &Fs) -> Result<(), BchError> {
    let trans = crate::btree_trans!(fs);
    let progress = Progress::recovery(fs, c"bch2_check_unreachable_inodes",
                                      &[c::btree_id::inodes], &[]);
    let mut seen = SubvolRootSeen::default();

    let mut iter = BtreeIter::new(&trans, c::btree_id::inodes, POS_MIN,
                                  BtreeIterFlags::PREFETCH | BtreeIterFlags::ALL_SNAPSHOTS);

    iter.for_each_commit(&trans, None, CommitFlags::NO_ENOSPC, |t, iter, k| {
        let t = progress.update(t, iter)?;
        check_unreachable_inode(t, k, &mut seen)
    })
}

crate::recovery_pass!(bch2_check_unreachable_inodes => check_unreachable_inodes);
