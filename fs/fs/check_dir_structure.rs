// SPDX-License-Identifier: GPL-2.0

//! check_subvolume_structure and check_directory_structure: earlier passes
//! fixed every other connectivity problem, so what's left is part of the tree
//! looping back on itself, or a subvolume whose path up doesn't reach the
//! root. The repair for both cuts the loop - removes the dirent naming it - and
//! reattaches in lost+found.
//!
//! Subvolumes walk their fs_path_parent chain to the root subvolume
//! (subvol_loop, subvol_unreachable); directories walk their backpointers to a
//! subvolume root (dir_loop).
//!
//! bi_depth makes the directory walk cheap: if every directory is deeper than
//! its parent, every chain decreases to a root, so each directory only checks
//! its own edge. One whose parent isn't shallower walks to the root, and the
//! path is renumbered after. Sound, because a loop has an edge that doesn't
//! decrease, and the walk from its child goes around and sees the repeat.
//!
//! Renumbering commits per inode in nested transactions - a path needn't fit
//! in one - which silently drops anything the outer attempt queued: nothing
//! may be queued on a path that renumbers. Today the walk only reads, and
//! repairs return before it.

use crate::btree::bkey::{pos, spos, BkeySC, POS_MIN, SPOS_MAX};
use crate::btree::bkey_buf::BkeyBuf;
use crate::btree::iter::{
    BtreeIter, BtreeIterFlags, CommitFlags, TransAttempt,
};
use crate::c;
use crate::errcode::{bch_errcode, BchError, Found};
use crate::fs::Fs;
use crate::init::error::id;
use crate::init::progress::Progress;
use crate::snapshots::subvolume;
use crate::dirent::{self, Dirent};
use crate::{inode, lostfound, namei};
use crate::util::alloc::{flags::GFP_KERNEL, KVVec};
use crate::util::Printbuf;
use crate::util::log::LogMsg;
use crate::{bch_err_fn, bch_err_msg, bch_warn, c_function_name, inode_fsck_err};

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
    if !inode.has_backpointer() {
        return Ok(());
    }

    let mut snapshot = inode.bi_snapshot;
    let mut iter = BtreeIter::uninit();

    // For a subvolume root, a missing parent subvolume reads as ENOENT here, like
    // a missing dirent - what reattach_subvol() is for. Nothing to remove is
    // success: the caller reattaches next.
    let Some(k) = namei::inode_get_dirent(t, &mut iter, inode, &mut snapshot).found()? else {
        return Ok(());
    };

    dirent_points_to_inode(t.fs(), k, inode)?;
    dirent::fsck_remove(t, k.k.p)
}

/// @root: the subvolume and its root inode.
fn reattach_subvol(t: &TransAttempt<'_, '_>, root: c::subvol_inum) -> Result<(), BchError> {
    let fs = t.fs();

    let mut inode = inode::find_by_inum_trans(t, root, c_function_name!())?;

    let ret = remove_backpointer(t, &mut inode);
    if !matches!(&ret, Err(e) if e.matches(c::ENOENT)) {
        bch_err_msg!(fs, ret, "removing dirent")?;
    }
    ret?;

    bch_err_msg!(fs, lostfound::reattach_inode(t, &mut inode),
                 "reattaching inode {}", inode.bi_inum)
}

fn check_subvol_path(t: &TransAttempt<'_, '_>, k: BkeySC<'_>) -> Result<(), BchError> {
    let fs = t.fs();

    let Some(start_sv) = subvolume::val(k) else {
        return Ok(());
    };

    // Unlinking zeroes fs_path_parent: a subvolume on its way to deletion has
    // no path by design, and isn't ours to reattach.
    if start_sv.state_decoded() != Some(c::bch_subvolume_state::SUBVOLUME_STATE_live) {
        return Ok(());
    }

    let mut subvol_path: KVVec<u32> = KVVec::new();
    let mut parent_iter = BtreeIter::new(t, c::btree_id::subvolumes, POS_MIN,
                                         BtreeIterFlags::empty());

    let start = c::subvol_inum {
        subvol: k.k.p.offset,
        inum:   start_sv.inode.get(),
    };

    // A copy: from the second step on the key is parent_iter's, whose unpacked
    // header looking up the parent overwrites
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

        inode::find_by_inum_trans(t, root, c_function_name!())?;

        let parent = sv.fs_path_parent.get();
        let inode_pos = spos(0, sv.inode.get(), sv.snapshot.get());

        if subvol_path.contains(&parent) {
            let mut buf = Printbuf::new();
            write!(buf, "subvolume loop: ");
            namei::inum_to_path(t, start, &mut buf)?;

            if inode_fsck_err!(t, inode_pos, id::subvol_loop, "{}", buf)? {
                return reattach_subvol(t, root);
            }
            return Ok(());
        }

        parent_iter.set_pos(pos(0, parent as u64));
        match parent_iter.peek_slot(t)? {
            Some(pk) if pk.arm::<c::Subvolume>().is_some() => {
                cur.reassemble(pk);
            }
            _ => {
                if inode_fsck_err!(t, inode_pos, id::subvol_unreachable,
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
    let fs = t.fs();
    let mut iter = BtreeIter::new(t, c::btree_id::inodes, spos(0, inum, snapshot),
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
    let fs = t.fs();

    // A nested attempt that restarted returns transaction_restart_nested, so the
    // outer one retries too
    for &inum in path.iter().rev() {
        bch_err_fn!(fs, t.nested(|t| bi_depth_renumber_one(t, inum, snapshot, new_bi_depth)))?;
        new_bi_depth += 1;
    }

    Ok(())
}

fn check_path_loop(t: &TransAttempt<'_, '_>, inode_k: BkeySC<'_>) -> Result<(), BchError> {
    let fs = t.fs();
    let mut path: KVVec<u64> = KVVec::new();
    let mut snapshot = inode_k.k.p.snapshot;
    let mut redo_bi_depth = false;
    let mut min_bi_depth = u32::MAX;

    let start = inode_k.k.p;

    let mut inode = inode::unpack(fs, inode_k);

    // At a redundant interior node delete_dead_snapshots will collapse, walk in
    // the live descendant's view: an interrupted collapse moved the dirents and
    // parents down there, and a half-moved dirent would read as unreachable.
    // No-op on a healthy fs.
    if let Some(terminal) = fs.snapshots.redundant_interior(snapshot) {
        snapshot = terminal;
    }

    // Not all_snapshots: the parent we want is the one visible in @snapshot,
    // and a filtered iterator looks in the snapshot it was initialized with -
    // set_pos() keeps it.
    let mut inode_iter = BtreeIter::new(t, c::btree_id::inodes, spos(0, 0, snapshot),
                                        BtreeIterFlags::empty());

    // After check_dirents, a missing dirent or backpointer is
    // check_unreachable_inodes'
    while inode.bi_subvol == 0 && inode.has_backpointer() {
        let mut dirent_iter = BtreeIter::new(t, c::btree_id::dirents,
                                             spos(inode.bi_dir, inode.bi_dir_offset, snapshot),
                                             BtreeIterFlags::empty());

        if let Err(e) = dirent_iter.peek_slot_typed::<c::Dirent>(t) {
            if !e.matches(c::ENOENT) {
                return Err(e);
            }

            // The dirent is gone, and the name with it, but bi_dir says which directory:
            // print its path. A restart propagates to the commit loop, unprinted.
            let mut msg = LogMsg::new(fs);
            write!(msg, "unreachable inode, naming dirent missing; was in directory ");
            if let Err(e) = namei::inum_snapshot_to_path(t, inode.bi_dir, snapshot, &mut msg) {
                msg.set_suppressed(true);
                return Err(e);
            }
            msg.newline();
            write!(msg, "{}", inode);
            return Err(e);
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

            namei::inum_snapshot_to_path(t, start.offset, start.snapshot, &mut buf)?;

            if fs.opts().verbose() {
                buf.newline();
                for i in path.iter() {
                    write!(buf, "{} ", i);
                }
            }

            if inode_fsck_err!(t, spos(0, inode.bi_inum, inode.bi_snapshot), id::dir_loop,
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

/// Check for directory loops.
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
            if inode::flags(k) & c::bch_inode_flags::BCH_INODE_unlinked.bits() != 0 {
                return Ok(());
            }

            check_path_loop(t, k)
        })
}

crate::recovery_pass!(bch2_check_subvolume_structure => check_subvolume_structure);
crate::recovery_pass!(bch2_check_directory_structure => check_directory_structure);
