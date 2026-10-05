// SPDX-License-Identifier: GPL-2.0

//! fsck: the check_extents and check_indirect_extents recovery passes.
//!
//! check_extents walks the extents btree, all snapshots, and checks each
//! extent against the versions of its inode it's visible in: that there is
//! one, of the right type (check_key_has_inode(), in InodeWalker::walk()),
//! that the extent doesn't overlap another visible in the same snapshot, and
//! that it isn't past i_size; then at the end of each inode, that i_sectors
//! matches the extents counted for each version.
//!
//! Each key commits its own repairs (for_each_attempt()), because what comes
//! after the commit - adding the extent to its inode versions' sector counts,
//! and recording where it ends for the overlap check - mustn't happen twice,
//! which a restart in the commit would do.
//!
//! Overlaps: extent_ends records, per snapshot, where the last extent seen
//! ended, with a copy of snapshots_seen as of that extent; an extent starting
//! before one of those ends overlaps it if the two are visible in some common
//! snapshot. One of the two is overwritten - an extent whiteout first, else
//! the extent in the older snapshot.
//!
//! check_i_sectors() is a nested transaction: counting an inode's sectors
//! spans too many extents for one, and its repairs commit, so it returns
//! transaction_restart_nested and the walk retries the key.
//!
//! check_indirect_extents is the part of this that applies to the reflink
//! btree: the overbig check, and dropping stale pointers.
//!
//! Changes from the C:
//!
//! - bch2_count_inode_sectors() returned an error as its count, and
//!   check_i_sectors_notnested() never checked for one: an error became
//!   i->count, and with the repair taken, bi_sectors. And it ran its own
//!   restart loop inside the caller's nested transaction (the C's XXX); now
//!   it walks without one, and a restart propagates, to the restart
//!   check_i_sectors() returns anyway. The flush after the walk, which called
//!   check_i_sectors_notnested() with no restart handler, now retries.
//!
//! - The past-i_size check and the sector count go over the inode versions
//!   the extent is visible in (InodeWalker::visible_mut(), C's
//!   for_each_visible_inode()); the C walked down from the version the key
//!   resolved to. They differ only under a redundant interior snapshot, where
//!   that version is found through the collapse terminal and the C skipped the
//!   versions between it and the key's own snapshot.
//!
//! - overlapping_extents_found() dereferenced the keys it looked up without
//!   checking they were there; a missing one is the internal error a
//!   mismatched one already was.
//!
//! XXX: the past-i_size check doesn't skip whiteout versions, as the sector
//! count does. A whiteout's bi_size is 0, so an extent still visible where
//! its inode is deleted - key_in_missing_inode declined, or the restore path,
//! which rewrites the version but leaves it marked a whiteout - is reported
//! as past the end of an inode with i_size 0, and punched from block 0.
//! Unverified; one for the error injection tests.

use crate::alloc::buckets::DiskReservation;
use crate::btree::bkey::{bkey_extent_whiteout, bpos_gt, pos, spos, BkeySC, POS_MIN};
use crate::btree::iter::{
    lockrestart_do, BtreeIter, BtreeIterFlags, BtreeTrans, CommitFlags, TransAttempt, TransBkey,
    TransResult, TransRet, UpdateTriggerFlags,
};
use crate::c;
use crate::c::bch_inode_flags::BCH_INODE_i_sectors_dirty;
use crate::check::{self, InodeWalker, SnapshotsSeen};
use crate::data::extents;
use crate::data::io_misc;
use crate::errcode::{bch_errcode, BchError};
use crate::fs::Fs;
use crate::init::damage;
use crate::init::error::id;
use crate::init::progress::Progress;
use crate::snapshots::snapshot;
use crate::util::alloc::{flags::GFP_KERNEL, kvvec_insert, KVVec};
use crate::util::Printbuf;
use crate::{bch_err, bch_err_fn, bch_err_ratelimited, fsck_err_on, inode_fsck_err};
use crate::namei;

/// Where the last extent seen in a snapshot ended.
struct ExtentEnd {
    snapshot: u32,
    offset:   u64,
    /// snapshots_seen as of that extent: what had overwritten it.
    seen:     SnapshotsSeen,
}

#[derive(Default)]
struct ExtentEnds {
    last_pos: c::bpos,
    /// Sorted by snapshot, one per snapshot.
    e:        KVVec<ExtentEnd>,
}

impl ExtentEnds {
    /// Record where @k ends.
    fn at(&mut self, fs: &Fs, seen: &SnapshotsSeen, k: c::bpos) -> Result<(), BchError> {
        let n = ExtentEnd { snapshot: k.snapshot, offset: k.offset, seen: seen.try_clone(fs)? };

        match self.e.iter().position(|i| i.snapshot >= k.snapshot) {
            Some(idx) if self.e[idx].snapshot == k.snapshot => self.e[idx] = n,
            Some(idx) => kvvec_insert(&mut self.e, idx, n)?,
            None => self.e.push(n, GFP_KERNEL)?,
        }
        Ok(())
    }
}

struct CheckExtents<'f> {
    res:         DiskReservation<'f>,
    s:           SnapshotsSeen,
    w:           InodeWalker,
    extent_ends: ExtentEnds,
}

/// The sectors allocated to inode @inum, as its version in @snapshot sees
/// them.
fn count_inode_sectors(trans: &BtreeTrans<'_>, inum: u64, snapshot: u32) -> Result<u64, BchError> {
    let mut sectors = 0;
    let mut iter = BtreeIter::new(trans, c::btree_id::extents, spos(inum, 0, snapshot),
                                  BtreeIterFlags::empty());

    iter.for_each_max_norestart(pos(inum, u64::MAX), |_, k| {
        if extents::bkey_extent_is_allocation(k.k) {
            sectors += k.k.size as u64;
        }
        Ok(core::ops::ControlFlow::<()>::Continue(()))
    })?;

    Ok(sectors)
}

/// The inode just walked: i_sectors against the extents counted for each
/// version. Repairs commit, each on its own.
fn check_i_sectors_notnested(trans: &BtreeTrans<'_>, w: &mut InodeWalker) -> Result<(), BchError> {
    let fs = trans.fs();
    let Some(inum) = w.cur_inum() else { return Ok(()) };
    let recalculate_sums = w.recalculate_sums();

    for i in w.inodes_mut() {
        if i.inode.bi_sectors == i.count {
            continue;
        }

        let mut buf = Printbuf::new();
        // Only the message's context: an inode we can't name still gets its
        // i_sectors fixed
        if let Err(e) = namei::inum_snapshot_to_path(trans, i.inode.bi_inum, i.inode.bi_snapshot,
                                                     &mut buf) {
            if e.matches(bch_errcode::BCH_ERR_transaction_restart) {
                return Err(e);
            }
        }
        write!(buf, "\n{}", i.inode);

        let count2 = count_inode_sectors(trans, inum, i.inode.bi_snapshot)?;

        if !recalculate_sums && i.count != count2 {
            bch_err_ratelimited!(fs, "fsck counted i_sectors wrong: got {} should be {}\n{}",
                                 i.count, count2, buf);
        }
        i.count = count2;

        if fsck_err_on!(trans, !i.inode.flag(BCH_INODE_i_sectors_dirty) &&
                        i.inode.bi_sectors != i.count,
                        id::inode_i_sectors_wrong,
                        "incorrect i_sectors: got {}, should be {}\n{}",
                        i.inode.bi_sectors, i.count, buf)? {
            i.inode.bi_sectors = i.count;
            crate::inode::fsck_write_inode(trans, &mut i.inode)?;
        }
    }

    Ok(())
}

/// check_i_sectors_notnested() from inside the key's attempt: its repairs
/// commit for themselves, spending the attempt - see self_committing() - and
/// the key is retried after.
fn check_i_sectors<'a, 't>(t: TransAttempt<'a, 't>, w: &mut InodeWalker) -> TransRet<'a, 't> {
    let fs = t.trans().fs();
    Ok(t.self_committing(|trans| bch_err_fn!(fs, check_i_sectors_notnested(trans, w)))?.0)
}

/// Overwrite @old, a copy of the extent at @iter, with @new where they
/// overlap - or, if @old is an extent whiteout, turn it into a whiteout.
fn overwrite_extent<'a, 't>(
    t:       TransAttempt<'a, 't>,
    iter:    &mut BtreeIter<'t>,
    mut old: TransBkey<'a, 't>,
    new:     BkeySC<'_>,
) -> TransRet<'a, 't> {
    if old.k().key_type() == c::bch_bkey_type::KEY_TYPE_extent_whiteout {
        old.k_mut().type_ = c::bch_bkey_type::KEY_TYPE_whiteout.0 as u8;
        t.update(iter, old, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)
    } else {
        let d = extents::durability_safe(t.trans().fs(), new);
        t.extra_disk_res_add(d.sectors_compressed as u64, d.nr_replicas as u32);

        t.update_extent_overwrite(iter, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE,
                                  BkeySC::from(old.k_i()), new)
    }
}

/// The extent that ended at @e1, in inode @inum, overlaps @pos2, the key
/// being checked: report it, and overwrite one with the other. Returns
/// whether it did.
fn overlapping_extents_found<'a, 't>(
    t:    TransAttempt<'a, 't>,
    res:  &DiskReservation<'_>,
    e1:   &mut ExtentEnd,
    inum: u64,
    pos2: c::bkey,
) -> TransResult<'a, 't, bool> {
    let trans = t.trans();
    let fs = trans.fs();
    let mut t = t;
    let pos1 = spos(inum, e1.offset, e1.snapshot);
    let end = pos(inum, u64::MAX);

    assert!(bpos_gt(pos1, pos2.start_pos()));

    let mut iter1 = BtreeIter::new(trans, c::btree_id::extents, pos1,
                                   BtreeIterFlags::ALL_SNAPSHOTS | BtreeIterFlags::NOT_EXTENTS);
    let mut iter2 = iter1.copy();

    let mut buf = Printbuf::new();
    write!(buf, "overlapping extents in ");
    namei::inum_snapshot_to_path(trans, inum, pos1.snapshot.min(pos2.p.snapshot), &mut buf)?;

    let k1 = iter1.peek_max(end)?;
    if let Some(k1) = k1 {
        write!(buf, "\n{}", k1.to_text(fs));
    }

    let Some(k1) = k1.filter(|k1| k1.k.p == pos1) else {
        bch_err!(fs, "overlapping_extents_found: error finding first overlapping extent when repairing, got{buf}\nwanted\n  {pos1}\n{pos2}");
        return Err(fs.err(bch_errcode::BCH_ERR_internal_fsck_err).into());
    };

    // iter2 starts at k1 too, and walks forward to pos2:
    iter2.peek_max(end)?;
    loop {
        iter2.advance();
        match iter2.peek_max(end)? {
            Some(k) if k.k.p < pos2.p => continue,
            _ => break,
        }
    }

    let k2 = iter2.peek_max(end)?;
    if let Some(k2) = k2 {
        write!(buf, "\n{}", k2.to_text(fs));
    }

    let Some(k2) = k2.filter(|k2| k2.k.p == pos2.p && k2.k.size == pos2.size) else {
        bch_err!(fs, "overlapping_extents_found: error finding second overlapping extent when repairing{buf}");
        return Err(fs.err(bch_errcode::BCH_ERR_internal_fsck_err).into());
    };

    let k1_whiteout = k1.key_type() == c::bch_bkey_type::KEY_TYPE_extent_whiteout;
    let k2_whiteout = k2.key_type() == c::bch_bkey_type::KEY_TYPE_extent_whiteout;
    let first = if k1_whiteout || k2_whiteout { k1_whiteout } else { pos1.snapshot >= pos2.p.snapshot };

    write!(buf, "\noverwriting {} extent", if first { "first" } else { "second" });

    if !inode_fsck_err!(trans, k1.k.p, id::extent_overlapping, "{buf}")? {
        return t.done(false);
    }

    // The update goes through the overwritten key's iterator, which its key
    // borrows: copy it out first
    t = if first {
        let old = t.bkey_make_mut_noupdate(k1)?;
        overwrite_extent(t, &mut iter1, old, k2)?
    } else {
        let old = t.bkey_make_mut_noupdate(k2)?;
        overwrite_extent(t, &mut iter2, old, k1)?
    };

    t = t.commit(Some(res), CommitFlags::NO_ENOSPC)?;

    if pos1.snapshot == pos2.p.snapshot {
        // We overwrote the first extent, and did the overwrite in the same
        // snapshot:
        e1.offset = pos2.start_offset();
    } else if pos1.snapshot > pos2.p.snapshot {
        // We overwrote the first extent in pos2's snapshot:
        e1.seen.add_inorder(fs, pos2.p.snapshot)?;
    } else {
        // We overwrote the second extent - restart check_extent() from the
        // top. (No need to say we fixed something: the retry's walk sees the
        // commit, and recounts.)
        return Err(t.restart(bch_errcode::BCH_ERR_transaction_restart_nested));
    }

    t.done(true)
}

/// Check @k against the extents before it that it could overlap. Returns
/// whether any overlap was repaired.
fn check_overlapping_extents<'a, 't>(
    t:           TransAttempt<'a, 't>,
    res:         &DiskReservation<'_>,
    s:           &mut SnapshotsSeen,
    extent_ends: &mut ExtentEnds,
    k:           BkeySC<'_>,
) -> TransResult<'a, 't, bool> {
    let trans = t.trans();
    let mut t = t;
    let mut fixed = false;

    // transaction restart, running again
    if extent_ends.last_pos == k.k.p {
        return t.done(false);
    }

    if extent_ends.last_pos.inode != k.k.p.inode {
        extent_ends.e.clear();
    }

    for i in extent_ends.e.iter_mut() {
        if i.offset <= k.start_offset() ||
           !check::ref_visible2(trans, k.k.p.snapshot, s, i.snapshot, &mut i.seen) {
            continue;
        }

        let (t2, f) = overlapping_extents_found(t, res, i, k.k.p.inode, *k.k)?;
        t = t2;
        fixed |= f;
    }

    extent_ends.last_pos = k.k.p;
    t.done(fixed)
}

/// Encoded (checksummed or compressed) extents are read whole, so they're
/// bounded by encoded_extent_max: one that isn't is a bug elsewhere, not
/// damage - reported, not repaired.
fn check_extent_overbig(fs: &Fs, k: BkeySC<'_>) {
    let max_sectors = fs.opts().encoded_extent_max() >> 9;

    for crc in extents::bkey_crcs(k) {
        if extents::crc_is_encoded(&crc) && crc.uncompressed_size > max_sectors {
            bch_err!(fs, "overbig encoded extent, please report this:\n  {}", k.to_text(fs));
        }
    }
}

/// Whether @k is past i_size in any inode version it's visible in. A repair
/// punches everything past i_size in that version's snapshot, which commits as
/// it goes and so ends with transaction_restart_nested: its own commit loop
/// always begins.
fn check_extent_past_end(
    trans: &BtreeTrans<'_>,
    st:    &mut CheckExtents<'_>,
    k:     BkeySC<'_>,
) -> Result<(), BchError> {
    let fs = trans.fs();
    let block_bytes = fs.block_bytes();
    let mut punch = None;

    for i in st.w.visible_mut(trans, &mut st.s, k.k.p.snapshot) {
        let last_block = i.inode.bi_size.div_ceil(block_bytes) * block_bytes >> 9;

        if fsck_err_on!(trans, k.k.p.offset > last_block &&
                        !extents::bkey_extent_is_reservation(fs, k),
                        id::extent_past_end_of_inode,
                        "extent type past end of inode {}:{}, i_size {}\n{}",
                        i.inode.bi_inum, i.inode.bi_snapshot, i.inode.bi_size,
                        k.to_text(fs))? {
            punch = Some((i.inode.bi_inum, i.inode.bi_snapshot, last_block));
            break;
        }
    }

    if let Some((inum, snapshot, last_block)) = punch {
        st.s.add_inorder(fs, snapshot)?;
        io_misc::fpunch_snapshot(trans, spos(inum, last_block, snapshot), pos(inum, u64::MAX))?;
    }

    Ok(())
}

fn check_extent<'a, 't>(
    t:    TransAttempt<'a, 't>,
    iter: &mut BtreeIter<'t>,
    k:    BkeySC<'_>,
    st:   &mut CheckExtents<'_>,
) -> TransRet<'a, 't> {
    let trans = t.trans();
    let fs = trans.fs();
    let mut t = t;

    // This walk can't use for_each_commit(): there's work to do after the
    // commit that can't handle a transaction restart
    if snapshot::check_key_has_snapshot(trans, iter, k)? {
        return t.commit(Some(&st.res), CommitFlags::NO_ENOSPC);
    }

    if st.w.cur_inum().is_some_and(|inum| inum != k.k.p.inode) {
        t = check_i_sectors(t, &mut st.w)?;
    }

    st.s.update(fs, c::btree_id::extents, k.k.p)?;

    t = st.w.walk(t, iter, k)?.0;

    if k.key_type() != c::bch_bkey_type::KEY_TYPE_whiteout {
        let (t2, fixed) = check_overlapping_extents(t, &st.res, &mut st.s, &mut st.extent_ends, k)?;
        t = t2;
        if fixed {
            st.w.set_recalculate_sums();
        }
    }

    if !bkey_extent_whiteout(k.k) {
        check_extent_past_end(trans, st, k)?;
    }

    check_extent_overbig(fs, k);
    t = extents::drop_stale_ptrs(t, iter, k)?;

    let p          = k.k.p;
    let size       = k.k.size as u64;
    let allocation = extents::bkey_extent_is_allocation(k.k);
    let whiteout   = k.key_type() == c::bch_bkey_type::KEY_TYPE_whiteout;

    t = t.commit(Some(&st.res), CommitFlags::NO_ENOSPC)?;

    if allocation {
        for i in st.w.visible_mut(trans, &mut st.s, p.snapshot) {
            if !i.whiteout {
                i.count += size;
            }
        }
    }

    if !whiteout {
        st.extent_ends.at(fs, &st.s, p)?;
    }

    Ok(t)
}

/// Walk extents: verify that extents have a corresponding S_ISREG inode, and
/// that i_size and i_sectors are consistent.
fn check_extents(fs: &Fs) -> Result<(), BchError> {
    // Before the walk, because the walk can't do it: to it an inode that lost
    // every extent it had looks sparse, not damaged. Only the ranges of the
    // nodes we lost can say which inodes those were.
    damage::record_lost_extents(fs);

    let trans = crate::btree_trans!(fs);
    let progress = Progress::recovery(fs, c"bch2_check_extents", &[c::btree_id::extents], &[]);
    let mut st = CheckExtents {
        res:         DiskReservation::new(fs),
        s:           SnapshotsSeen::new(),
        w:           InodeWalker::new(),
        extent_ends: ExtentEnds::default(),
    };

    let mut iter = BtreeIter::new(&trans, c::btree_id::extents,
                                  pos(c::BCACHEFS_ROOT_INO as u64, 0),
                                  BtreeIterFlags::PREFETCH | BtreeIterFlags::ALL_SNAPSHOTS);

    iter.for_each_attempt(&trans, |t, iter, k| {
        st.res.put();
        let t = progress.update(t, iter)?;
        check_extent(t, iter, k, &mut st)
    })?;

    // The last inode's i_sectors, as in the walk, with a restart retrying it
    // here: whatever it repaired is in memory, so the retry has nothing left
    // to do.
    lockrestart_do(&trans, |t| Ok((check_i_sectors(t, &mut st.w)?, ())))
}

fn check_indirect_extents(fs: &Fs) -> Result<(), BchError> {
    let res = DiskReservation::new(fs);
    let trans = crate::btree_trans!(fs);
    let progress = Progress::recovery(fs, c"bch2_check_indirect_extents",
                                      &[c::btree_id::reflink], &[]);

    let mut iter = BtreeIter::new(&trans, c::btree_id::reflink, POS_MIN,
                                  BtreeIterFlags::PREFETCH);

    iter.for_each_commit(&trans, Some(&res), CommitFlags::NO_ENOSPC, |t, iter, k| {
        res.put();
        let t = progress.update(t, iter)?;
        check_extent_overbig(fs, k);
        extents::drop_stale_ptrs(t, iter, k)
    })
}

crate::recovery_pass!(bch2_check_extents => check_extents);
crate::recovery_pass!(bch2_check_indirect_extents => check_indirect_extents);
