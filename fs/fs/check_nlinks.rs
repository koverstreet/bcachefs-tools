// SPDX-License-Identifier: GPL-2.0

//! fsck: check inode link counts - the check_nlinks recovery pass.
//!
//! Directories can't have hardlinks - backpointer and directory structure
//! checks cover them - so only non-directory inodes with a nonzero link count
//! are counted. A range of inode numbers at a time, as many as a table of
//! (inum, snapshot, count) fits in memory:
//!
//!  1. collect the inodes in the range;
//!  2. walk every dirent, counting each against the inodes it points at that
//!     are visible to it;
//!  3. walk the inodes again, fixing any whose link count doesn't match.

use crate::btree::bkey::{pos, spos, BkeySC, POS_MIN};
use crate::btree::iter::{
    BtreeIter, BtreeIterFlags, BtreeTrans, CommitFlags, TransAttempt,
};
use crate::c;
use crate::check::SnapshotsSeen;
use crate::dirent::{Dirent, DirentTarget};
use crate::errcode::{bch_errcode, BchError};
use crate::fs::Fs;
use crate::fsck_err_on;
use crate::init::error::id;
use crate::inode;
use crate::util::alloc::{flags::GFP_KERNEL, KVVec};
use crate::{bch_err, bch_err_fn};
use core::cmp::max;
use core::ops::ControlFlow;

#[derive(Clone, Copy)]
struct Nlink {
    inum:     u64,
    snapshot: u32,
    count:    u32,
}

fn add_nlink(fs: &Fs, t: &mut KVVec<Nlink>, inum: u64, snapshot: u32) -> Result<(), BchError> {
    if t.len() == t.capacity() {
        let new_size = max(128, t.capacity() * 2);

        if t.reserve(new_size - t.len(), GFP_KERNEL).is_err() {
            bch_err!(fs, "fsck: error allocating memory for nlink_table, size {}", new_size);
            return fs.throw(bch_errcode::BCH_ERR_ENOMEM_fsck_add_nlink);
        }
    }

    // Can't fail: there's room.
    t.push(Nlink { inum, snapshot, count: 0 }, GFP_KERNEL)
        .map_err(|_| fs.err(bch_errcode::BCH_ERR_ENOMEM_fsck_add_nlink))
}

fn inc_link(
    trans:       &BtreeTrans<'_>,
    s:           &mut SnapshotsSeen,
    links:       &mut [Nlink],
    range_start: u64,
    range_end:   u64,
    inum:        u64,
    snapshot:    u32,
) {
    if inum < range_start || inum >= range_end {
        return;
    }

    let first = links.partition_point(|l| l.inum < inum);

    for link in links[first..].iter_mut().take_while(|l| l.inum == inum) {
        if s.ref_visible(trans, snapshot, link.snapshot) {
            link.count += 1;
            if link.snapshot >= snapshot {
                break;
            }
        }
    }
}

fn is_dir(u: &c::bch_inode_unpacked) -> bool {
    u.bi_mode as u32 & c::S_IFMT == c::S_IFDIR
}

fn iter_flags() -> BtreeIterFlags {
    BtreeIterFlags::INTENT | BtreeIterFlags::PREFETCH | BtreeIterFlags::ALL_SNAPSHOTS
}

fn check_nlinks_find_hardlinks(
    fs:    &Fs,
    t:     &mut KVVec<Nlink>,
    start: u64,
    end:   &mut u64,
) -> Result<(), BchError> {
    let trans = crate::btree_trans!(fs);
    let mut iter = BtreeIter::new(&trans, c::btree_id::inodes, pos(0, start), iter_flags());

    let ret = iter.for_each(&trans, |_, k| {
        if !inode::bkey_is_inode(k.k) {
            return Ok(ControlFlow::Continue(()));
        }

        let u = inode::unpack(fs, k);

        // Backpointer and directory structure checks are sufficient for
        // directories, since they can't have hardlinks:
        if is_dir(&u) {
            return Ok(ControlFlow::Continue(()));
        }

        // Previous passes ensured that bi_nlink is nonzero if it had multiple
        // hardlinks:
        if u.bi_nlink == 0 {
            return Ok(ControlFlow::Continue(()));
        }

        // Table full: this range ends here, the next starts at this inode.
        if add_nlink(fs, t, k.k.p.offset, k.k.p.snapshot).is_err() {
            *end = k.k.p.offset;
            return Ok(ControlFlow::Break(()));
        }

        Ok(ControlFlow::Continue(()))
    });

    bch_err_fn!(fs, ret)
}

fn check_nlinks_walk_dirents(
    fs:          &Fs,
    links:       &mut KVVec<Nlink>,
    range_start: u64,
    range_end:   u64,
) -> Result<(), BchError> {
    let trans = crate::btree_trans!(fs);
    let mut s = SnapshotsSeen::new();
    let mut iter = BtreeIter::new(&trans, c::btree_id::dirents, POS_MIN, iter_flags());

    let ret = iter.for_each(&trans, |_, k| {
        s.update(k.k.p)?;

        if let Some(d) = Dirent::new(k) {
            match d.target() {
                DirentTarget::Inode(inum) if d.d_type() as u32 != c::DT_DIR =>
                    inc_link(&trans, &mut s, links, range_start, range_end,
                             inum, k.k.p.snapshot),
                _ => {}
            }
        }

        Ok(())
    });

    bch_err_fn!(fs, ret)
}

fn check_nlinks_update_inode(
    t:         &TransAttempt<'_, '_>,
    k:         BkeySC<'_>,
    links:     &[Nlink],
    idx:       &mut usize,
) -> Result<(), BchError> {
    if !inode::bkey_is_inode(k.k) {
        return Ok(());
    }

    let mut u = inode::unpack(t.fs(), k);

    if is_dir(&u) || u.bi_nlink == 0 {
        return Ok(());
    }

    // Every inode that got here is in the table - the first pass collected
    // exactly these - so this stops on it before running off the end, which
    // the indexing would catch.
    while (links[*idx].inum, links[*idx].snapshot) < (k.k.p.offset, k.k.p.snapshot) {
        *idx += 1;
    }
    let link = links[*idx];

    let nlink = u.nlink();
    let unlinked = u.bi_flags & c::bch_inode_flags::BCH_INODE_unlinked.bits() as u32 != 0;

    if fsck_err_on!(t, nlink != link.count || (unlinked && u.bi_nlink != 0),
                    id::inode_wrong_nlink,
                    "inode has wrong i_nlink ({}, should be {})\n{}", nlink, link.count, u)? {
        u.set_nlink(link.count);
        return inode::fsck_write(t, &mut u);
    }

    Ok(())
}

fn check_nlinks_update_hardlinks(
    fs:          &Fs,
    links:       &KVVec<Nlink>,
    range_start: u64,
    range_end:   u64,
) -> Result<(), BchError> {
    let trans = crate::btree_trans!(fs);
    let mut idx = 0;
    let mut iter = BtreeIter::new(&trans, c::btree_id::inodes, pos(0, range_start), iter_flags());

    // range_end is past range_start, so this ends with its last inode:
    let end = spos(0, range_end - 1, u32::MAX);

    let ret = iter.for_each_max_commit(&trans, end, iter_flags(), None,
        CommitFlags::NO_ENOSPC,
        |t, _, k| check_nlinks_update_inode(t, k, links, &mut idx));

    if let Err(e) = ret {
        bch_err!(fs, "error in fsck walking inodes: {}", e.msg());
        return Err(e);
    }

    Ok(())
}

fn check_nlinks(fs: &Fs) -> Result<(), BchError> {
    let mut links = KVVec::new();
    let mut next_iter_range_start = 0;

    loop {
        let this_iter_range_start = next_iter_range_start;
        next_iter_range_start = u64::MAX;

        check_nlinks_find_hardlinks(fs, &mut links,
                                    this_iter_range_start,
                                    &mut next_iter_range_start)?;

        check_nlinks_walk_dirents(fs, &mut links,
                                  this_iter_range_start,
                                  next_iter_range_start)?;

        check_nlinks_update_hardlinks(fs, &links,
                                      this_iter_range_start,
                                      next_iter_range_start)?;

        links.clear();

        if next_iter_range_start == u64::MAX {
            return Ok(());
        }
    }
}

crate::recovery_pass!(bch2_check_nlinks => check_nlinks);
