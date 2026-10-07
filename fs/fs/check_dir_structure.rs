// SPDX-License-Identifier: GPL-2.0

//! fsck: directory and subvolume structure - the check_subvolume_structure and
//! check_directory_structure recovery passes. Every other connectivity problem
//! has been fixed by earlier passes; these two look for what's left: a part of
//! the tree that loops back on itself, or a subvolume whose path up doesn't
//! reach the root. The repair for both is to cut the loop - remove the dirent
//! naming it - and reattach in lost+found.
//!
//!  - check_subvolume_structure walks each live subvolume up its
//!    fs_path_parent chain to the root subvolume, reporting a loop
//!    (subvol_loop) or a parent that isn't a subvolume (subvol_unreachable).
//!  - check_directory_structure walks each directory up its backpointers
//!    (bi_dir, bi_dir_offset) to a subvolume root, reporting a loop
//!    (dir_loop).
//!
//! bi_depth makes the directory walk cheap: if every directory is deeper than
//! its parent, every chain strictly decreases to a root and there are no
//! loops, so each directory only checks its own edge. A directory whose parent
//! isn't shallower walks to the root, trusting no depth past that one, and the
//! walked path is renumbered after. Soundness: a loop has an edge that doesn't
//! decrease, and the walk starting at its child takes no early exit, so it goes
//! around and sees the repeat. bi_depth_renumber commits as it goes - a nested
//! transaction inside the commit loop, one commit per inode, since a path's
//! length is unbounded and needn't fit in one transaction. That drops anything
//! the outer attempt has queued, silently - the dropped-updates check is off
//! for nested transactions - so nothing may be queued on a path that
//! renumbers: today the walk only reads, and repairs return before it.
//!
//! Keys live partly in their iterator - a packed key's header is unpacked into
//! it - so a key is only good until its iterator moves: check_subvol_path()
//! copies each step's key before looking up its parent.

use crate::btree::bkey::{pos, spos, BkeySC, POS_MIN, SPOS_MAX};
use crate::btree::bkey_buf::BkeyBuf;
use crate::btree::iter::{
    BtreeIter, BtreeIterFlags, BtreeTrans, CommitFlags, TransAttempt,
};
use crate::c;
use crate::errcode::{bch_errcode, BchError, Found};
use crate::fs::Fs;
use crate::init::error::id;
use crate::init::progress::Progress;
use crate::snapshots::{snapshot, subvolume};
use crate::dirent::{self, Dirent};
use crate::{inode, lostfound, namei};
use crate::util::alloc::{flags::GFP_KERNEL, KVVec};
use crate::util::Printbuf;
use crate::{bch_err, bch_err_fn, bch_err_msg, bch_warn, c_function_name, inode_fsck_err};

/// @k: a dirent, from a dirent lookup.
fn dirent_points_to_inode(
    fs:    &Fs,
    k:     BkeySC<'_>,
    inode: &mut c::bch_inode_unpacked,
) -> Result<(), BchError> {
    let d = Dirent::new(k).expect("dirent lookups only return dirents");

    if namei::dirent_points_to_inode(d, inode) {
        return Ok(());
    }

    bch_warn!(fs, "{}", namei::dirent_inode_mismatch(fs, k, inode));
    fs.throw(bch_errcode::BCH_ERR_ENOENT_dirent_doesnt_match_inode)
}

fn remove_backpointer(
    t:     &TransAttempt<'_, '_>,
    inode: &mut c::bch_inode_unpacked,
) -> Result<(), BchError> {
    let trans: &BtreeTrans<'_> = t;
    if !inode.has_backpointer() {
        return Ok(());
    }

    let mut snapshot = inode.bi_snapshot;
    let mut iter = BtreeIter::uninit();

    // For a subvolume root, get_dirent() resolves the dirent through
    // bi_parent_subvol - so a missing parent subvolume lands here as an
    // ENOENT, like a not-found dirent. That is exactly the state
    // reattach_subvol() is called for.
    //
    // Nothing to remove is success: the caller's next move is to reattach,
    // and it can't if we hand it an error.
    let Some(k) = namei::inode_get_dirent(t, &mut iter, inode, &mut snapshot).found()? else {
        return Ok(());
    };

    dirent_points_to_inode(trans.fs(), k, inode)?;
    dirent::fsck_remove(t, k.k.p)
}

/// @root: the subvolume and its root inode.
fn reattach_subvol(t: &TransAttempt<'_, '_>, root: c::subvol_inum) -> Result<(), BchError> {
    let trans = t.trans();
    let fs = trans.fs();

    let mut inode = inode::find_by_inum_trans(trans, root, c_function_name!())?;

    let ret = remove_backpointer(t, &mut inode);
    if !matches!(&ret, Err(e) if e.matches(c::ENOENT)) {
        bch_err_msg!(fs, ret, "removing dirent")?;
    }
    ret?;

    bch_err_msg!(fs, lostfound::reattach_inode(t, &mut inode),
                 "reattaching inode {}", inode.bi_inum)
}

fn check_subvol_path(t: &TransAttempt<'_, '_>, k: BkeySC<'_>) -> Result<(), BchError> {
    let trans = t.trans();
    let fs = trans.fs();

    let Some(start_sv) = subvolume::val(k) else {
        return Ok(());
    };

    // Unlinking zeroes fs_path_parent: a subvolume on its way to deletion has
    // no path by design, and isn't ours to reattach.
    if start_sv.state() != Some(c::bch_subvolume_state::SUBVOLUME_STATE_live) {
        return Ok(());
    }

    let mut subvol_path: KVVec<u32> = KVVec::new();
    let mut parent_iter = BtreeIter::new(trans, c::btree_id::subvolumes, POS_MIN,
                                         BtreeIterFlags::empty());

    let start = c::subvol_inum {
        subvol: k.k.p.offset,
        inum:   start_sv.inode.get(),
    };

    // Each step works on a copy of its subvolume key: from the second step on
    // the key came from parent_iter, which holds its unpacked header - and
    // looking up the parent overwrites it.
    let mut cur = BkeyBuf::new();
    cur.reassemble(k);

    loop {
        let s = cur.sc();
        let sv = subvolume::val(s).expect("cur only ever holds subvolume keys");

        if s.k.p.offset == c::BCACHEFS_ROOT_SUBVOL as u64 {
            return Ok(());
        }

        subvol_path.push(s.k.p.offset as u32, GFP_KERNEL)?;

        let root = c::subvol_inum { subvol: s.k.p.offset, inum: sv.inode.get() };

        inode::find_by_inum_trans(trans, root, c_function_name!())?;

        let parent = sv.fs_path_parent.get();
        let inode_pos = spos(0, sv.inode.get(), sv.snapshot.get());

        if subvol_path.contains(&parent) {
            let mut buf = Printbuf::new();
            write!(buf, "subvolume loop: ");
            namei::inum_to_path(trans, start, &mut buf)?;

            if inode_fsck_err!(trans, inode_pos, id::subvol_loop, "{}", buf)? {
                return reattach_subvol(t, root);
            }
            return Ok(());
        }

        parent_iter.set_pos(pos(0, parent as u64));
        match parent_iter.peek_slot(t)? {
            Some(pk) if pk.as_subvolume().is_some() => {
                cur.reassemble(pk);
            }
            _ => {
                if inode_fsck_err!(trans, inode_pos, id::subvol_unreachable,
                                   "unreachable subvolume {}", s.to_text(fs))? {
                    return reattach_subvol(t, root);
                }
                return Ok(());
            }
        }
    }
}

fn check_subvolume_structure(fs: &Fs) -> Result<(), BchError> {
    let trans = crate::btree_trans!(fs);
    let progress = Progress::recovery(fs, c"bch2_check_subvolume_structure",
                                      &[c::btree_id::subvolumes], &[]);

    let mut iter = BtreeIter::new(&trans, c::btree_id::subvolumes, POS_MIN,
                                  BtreeIterFlags::PREFETCH);

    iter.for_each_commit(&trans, None, CommitFlags::NO_ENOSPC,
        |t, iter, k| {
            progress.update(t, iter)?;
            check_subvol_path(t, k)
        })
}

fn bi_depth_renumber_one(
    t:         &TransAttempt<'_, '_>,
    inum:      u64,
    snapshot:  u32,
    new_depth: u32,
) -> Result<(), BchError> {
    let fs = t.trans().fs();
    let mut iter = BtreeIter::new(t.trans(), c::btree_id::inodes, spos(0, inum, snapshot),
                                  BtreeIterFlags::empty());

    let Some(k) = iter.peek_slot(t)?.filter(|k| inode::bkey_is_inode(k.k)) else {
        return Err(bch_errcode::BCH_ERR_ENOENT_inode.into());
    };

    let mut inode = inode::unpack(fs, k);

    if inode.bi_depth != new_depth {
        inode.bi_depth = new_depth;
        inode::fsck_write(t, &mut inode)?;
        t.commit(None, CommitFlags::empty())?;
    }

    Ok(())
}

fn bi_depth_renumber(
    t:                &TransAttempt<'_, '_>,
    path:             &[u64],
    snapshot:         u32,
    mut new_bi_depth: u32,
) -> Result<(), BchError> {
    let fs = t.trans().fs();

    // Each nested attempt that restarted ends with transaction_restart_nested,
    // as nested_lockrestart_do() did; the C's closing trans_was_restarted()
    // can't see a restart those didn't.
    for &inum in path.iter().rev() {
        bch_err_fn!(fs, t.nested(|t| bi_depth_renumber_one(t, inum, snapshot, new_bi_depth)))?;
        new_bi_depth += 1;
    }

    Ok(())
}

fn check_path_loop(t: &TransAttempt<'_, '_>, inode_k: BkeySC<'_>) -> Result<(), BchError> {
    let trans = t.trans();
    let fs = trans.fs();
    let mut path: KVVec<u64> = KVVec::new();
    let mut snapshot = inode_k.k.p.snapshot;
    let mut redo_bi_depth = false;
    let mut min_bi_depth = u32::MAX;

    let start = inode_k.k.p;

    let mut inode = inode::unpack(fs, inode_k);

    // If this inode sits at a redundant interior snapshot node that
    // delete_dead_snapshots will collapse into a live descendant, do the whole
    // path traversal in the descendant's view: an interrupted collapse migrated
    // the naming dirents and parent inodes down there, and that's the state the
    // subtree is converging on - otherwise a half-migrated dirent reads as a
    // spurious unreachable-inode. Computed once (the walk stays in one
    // snapshot); no-op on a healthy fs.
    if let Some(terminal) = snapshot::redundant_interior(fs, snapshot) {
        snapshot = terminal;
    }

    // Not all_snapshots: the parent we want is the one visible in @snapshot,
    // and a filtered iterator looks in the snapshot it was initialized with -
    // set_pos() keeps it.
    let mut inode_iter = BtreeIter::new(trans, c::btree_id::inodes, spos(0, 0, snapshot),
                                        BtreeIterFlags::empty());

    // If we're running full fsck, check_dirents() will have already ran, and we
    // shouldn't see any missing alloc/backpointers here - otherwise that's
    // handled separately, by check_unreachable_inodes
    while inode.bi_subvol == 0 && inode.has_backpointer() {
        let mut dirent_iter = BtreeIter::new(trans, c::btree_id::dirents,
                                             spos(inode.bi_dir, inode.bi_dir_offset, snapshot),
                                             BtreeIterFlags::empty());

        if let Err(e) = dirent_iter.peek_slot_typed(t, c::bch_bkey_type::KEY_TYPE_dirent) {
            if !e.matches(c::ENOENT) {
                return Err(e.into());
            }

            // The naming dirent is gone, so we can't recover the filename -
            // but bi_dir still tells us which directory the inode was in.
            // Print that path so the user can see what was lost. A restart
            // from the path walk just propagates to the enclosing
            // for_each_..._commit loop.
            let mut buf = Printbuf::new();
            write!(buf, "unreachable inode, naming dirent missing; was in directory ");
            namei::inum_snapshot_to_path(trans, inode.bi_dir, snapshot, &mut buf)?;
            buf.newline();
            write!(buf, "{}", inode);
            bch_err!(fs, "{}", buf);
            return Err(e.into());
        }

        path.push(inode.bi_inum, GFP_KERNEL)?;

        inode_iter.set_pos(spos(0, inode.bi_dir, snapshot));
        // Should have been caught in dirents pass
        let parent_inode = bch_err_msg!(fs, match inode_iter.peek_slot(t) {
            Ok(Some(k)) if inode::bkey_is_inode(k.k) => Ok(inode::unpack(fs, k)),
            ret => Err(ret.err()
                .unwrap_or(bch_errcode::BCH_ERR_ENOENT_inode.into())),
        }, "error looking up parent directory")?;

        min_bi_depth = parent_inode.bi_depth;

        // Depths decrease towards the root, so a shallower parent means the
        // rest of the path has been checked - unless this walk has already
        // crossed a depth that's wrong. Around a loop the depths can't all
        // decrease, so trusting them past a wrong one always stops the walk
        // before it comes back around.
        if !redo_bi_depth &&
            parent_inode.bi_depth < inode.bi_depth &&
            min_bi_depth < u16::MAX as u32
        {
            break;
        }

        inode = parent_inode;
        redo_bi_depth = true;

        if path.contains(&inode.bi_inum) {
            let mut buf = Printbuf::new();
            write!(buf, "directory structure loop in snapshot {}: ", snapshot);

            namei::inum_snapshot_to_path(trans, start.offset, start.snapshot, &mut buf)?;

            if fs.opts().verbose() {
                buf.newline();
                for i in path.iter() {
                    write!(buf, "{} ", i);
                }
            }

            if inode_fsck_err!(trans, spos(0, inode.bi_inum, inode.bi_snapshot), id::dir_loop,
                               "{}", buf)? {
                bch_err_msg!(fs, remove_backpointer(t, &mut inode), "removing dirent")?;

                // Done with this path: it was a loop, there are no depths
                // along it to renumber - and on error or restart, nothing more
                // may run in this transaction.
                return bch_err_msg!(fs, lostfound::reattach_inode(t, &mut inode),
                                    "reattaching inode {}", inode.bi_inum);
            }

            break;
        }
    }

    if inode.bi_subvol != 0 {
        min_bi_depth = 0;
    }

    if redo_bi_depth {
        return bi_depth_renumber(t, &path, snapshot, min_bi_depth);
    }

    Ok(())
}

/// Check for loops in the directory structure: all other connectivity issues
/// have been fixed by prior passes
fn check_directory_structure(fs: &Fs) -> Result<(), BchError> {
    let trans = crate::btree_trans!(fs);
    let mut iter = BtreeIter::new(&trans, c::btree_id::inodes, SPOS_MAX,
        BtreeIterFlags::INTENT | BtreeIterFlags::PREFETCH | BtreeIterFlags::ALL_SNAPSHOTS);

    iter.for_each_reverse_commit(&trans, POS_MIN, None,
        CommitFlags::NO_ENOSPC,
        |t, _, k| {
            if inode::mode(k) & c::S_IFMT != c::S_IFDIR {
                return Ok(());
            }

            // check_inodes has already stripped BCH_INODE_unlinked from every
            // directory but the root of an unlinked subvolume, and a subvolume
            // root ends every walk - so skipping these can't hide a directory
            // loop:
            if inode::flags(k) & c::bch_inode_flags::BCH_INODE_unlinked.bits() as u32 != 0 {
                return Ok(());
            }

            check_path_loop(t, k)
        })
}

crate::recovery_pass!(bch2_check_subvolume_structure => check_subvolume_structure);
crate::recovery_pass!(bch2_check_directory_structure => check_directory_structure);
