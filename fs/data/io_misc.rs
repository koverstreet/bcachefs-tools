// SPDX-License-Identifier: GPL-2.0

//! fpunch, truncate, finsert/fcollapse: data/io_misc.c, but for fallocate,
//! which is C still.
//!
//! fpunch deletes a range of an inode's extents in a loop of its own: a
//! transaction restart from fpunch_at() means it had to drop locks along the
//! way, and is the caller's to take as such.
//!
//! truncate and finsert/fcollapse are logged ops (fs/logged_ops.rs): the op
//! key goes in first, each step commits with the op advanced to it, and
//! recovery resumes an op a crash interrupted. Logged ops aren't atomic w.r.t.
//! snapshot creation - creating a snapshot while one is in progress, then
//! crashing, resumes it in only one of the snapshots - so both run under
//! snapshots.create_lock, for read.

use crate::btree::bkey::prelude::*;
use crate::alloc::buckets::DiskReservation;
use crate::btree::bkey::{bkey_lt, bpos_max, pos, spos, BkeyI, BkeySC, BkeyWhole};
use crate::btree::iter::{commit_do, is_restart, lockrestart_do, BtreeIter, BtreeIterFlags, BtreeTrans,
                         CommitFlags, CommitOpts, TransAttempt, TransBkey, UpdateTriggerFlags};
use crate::c;
use crate::data::extents::Extents;
use crate::errcode::{bch_errcode, ret_to_c, BchError};
use crate::fs::Fs;
use crate::snapshots::subvolume;
use crate::util::ffi::Opaque;
use crate::util::{max_of, Printbuf};
use crate::{bch_err_fn, inode, logged_ops};
use core::ffi::c_int;
use cstruct_macros::c_export;

/// MAX_LFS_FILESIZE: as far as the page cache indexes - a 32 bit kernel's
/// ULONG_MAX pages, else LLONG_MAX.
#[cfg(all(kernel, target_pointer_width = "32"))]
const MAX_LFS_FILESIZE: u64 = (usize::MAX as u64) << kernel::page::PAGE_SHIFT;
#[cfg(not(all(kernel, target_pointer_width = "32")))]
const MAX_LFS_FILESIZE: u64 = i64::MAX as u64;

/// A deleted key at @p - size 0, for the caller to give one.
fn delete_key<'a, 't>(t: &TransAttempt<'a, 't>, p: c::bpos) -> Result<TransBkey<'a, 't>, BchError> {
    t.bkey_alloc_init(0, c::bch_bkey_type::KEY_TYPE_deleted.0 as u8, p)
}

// ── fpunch ───────────────────────────────────────────────────────────────

/// For fsck: delete the extents from @start to @end in @start's snapshot, in a
/// commit loop of its own. Called through TransAttempt::close(), which does
/// C's bch2_fpunch_snapshot()'s bookends: commits what the caller queued first
/// - an fsck_err()'s log of the repair, which the repair won't re-trigger to
/// queue again once the extents are gone - and returns
/// transaction_restart_nested after.
pub fn fpunch_snapshot(trans: &BtreeTrans<'_>, start: c::bpos, end: c::bpos) -> Result<(), BchError> {
    let fs = trans.fs();
    let res = DiskReservation::new(fs);
    let mut iter = BtreeIter::new(trans, c::btree_id::extents, start, BtreeIterFlags::empty());

    iter.for_each_max_commit(trans, end, BtreeIterFlags::empty(), Some(&res), CommitFlags::NO_ENOSPC,
                             |t, iter, _| {
        let mut delete = delete_key(t, iter.pos())?;

        /* create the biggest key we can */
        let max_sectors = max_of(&delete.k().size) & (!0 << fs.block_bits());
        delete.k_mut().resize(max_sectors);
        let delete = delete.arm::<Extents>().expect("sized just above")
            .cut_back(end).expect("peek_max() of an extents btree stops at end: iter.pos() < end");

        // trim_atomic() only reads @iter: it walks a copy
        t.result(unsafe { c::bch2_extent_trim_atomic(t.raw(), iter.raw(), delete.as_ptr()) })?;
        t.update(iter, &delete, UpdateTriggerFlags::empty())
    })
}

/// Delete @inum's extents from @iter's position to @end, a commit each:
/// transaction_restart if it had to drop locks along the way.
pub fn fpunch_at<'t>(
    trans:               &'t BtreeTrans<'t>,
    iter:                &mut BtreeIter<'t>,
    inum:                c::subvol_inum,
    end:                 u64,
    mut i_sectors_delta: Option<&mut i64>,
) -> Result<(), BchError> {
    let fs = trans.fs();
    let end_pos = pos(inum.inum, end);
    let mut restarted = Ok(());

    loop {
        let res = DiskReservation::new(fs);
        let t = trans.begin();

        let ret = (|| -> Result<bool, BchError> {
            let snapshot = subvolume::get_snapshot(&t, inum.subvol as u32)?;
            iter.set_snapshot(snapshot);

            /*
             * peek_max() doesn't have ideal semantics for extents:
             */
            let Some(k) = iter.peek_max(&t, end_pos)? else { return Ok(true) };
            let k_end = k.k.p.offset;

            let mut delete = delete_key(&t, iter.pos())?;
            delete.k_mut().resize((end.min(k_end) - iter.pos().offset) as u32);

            let delta = i_sectors_delta.as_deref_mut().map_or(core::ptr::null_mut(), core::ptr::from_mut);
            t.result(unsafe {
                c::bch2_extent_update(t.raw(), inum, iter.raw_mut(), delete.as_ptr(),
                                      delete.k().u64s as u32, res.as_mut_ptr(), 0, delta, false,
                                      0, core::ptr::null_mut())
            })?;
            Ok(false)
        })();

        match ret {
            Ok(true)                 => return restarted,
            Ok(false)                => {}
            Err(e) if is_restart(&e) => restarted = Err(e),
            Err(e)                   => return Err(e),
        }
    }
}

/// # Safety
/// @i_sectors_delta is NULL, or valid for the call.
#[c_export]
pub unsafe extern "C" fn bch2_fpunch_at<'t>(
    trans:           &'t BtreeTrans<'t>,
    iter:            &mut BtreeIter<'t>,
    inum:            c::subvol_inum,
    end:             u64,
    i_sectors_delta: *mut i64,
) -> c_int {
    ret_to_c(fpunch_at(trans, iter, inum, end, unsafe { i_sectors_delta.as_mut() }))
}

/// Delete @inum's extents from @start to @end.
pub fn fpunch(fs: &Fs, inum: c::subvol_inum, start: u64, end: u64, i_sectors_delta: &mut i64)
    -> Result<(), BchError>
{
    let trans = crate::btree_trans!(fs);
    let mut iter = BtreeIter::new(&trans, c::btree_id::extents, pos(inum.inum, start), BtreeIterFlags::INTENT);

    match fpunch_at(&trans, &mut iter, inum, end, Some(i_sectors_delta)) {
        Err(e) if is_restart(&e) => Ok(()),
        ret                      => ret,
    }
}

/// # Safety
/// @i_sectors_delta is valid for the call.
#[c_export]
pub unsafe extern "C" fn bch2_fpunch(
    c:               &Opaque<c::bch_fs>,
    inum:            c::subvol_inum,
    start:           u64,
    end:             u64,
    i_sectors_delta: *mut i64,
) -> c_int {
    ret_to_c(fpunch(Fs::from_c(c), inum, start, end, unsafe { &mut *i_sectors_delta }))
}

// ── truncate ─────────────────────────────────────────────────────────────

#[cold]
#[c_export]
pub fn bch2_logged_op_truncate_to_text(out: &mut Printbuf, _c: &Opaque<c::bch_fs>, k: BkeySC<'_>) {
    let op = c::LoggedOpTruncate::val_copy_pad(k).expect("a truncate op");

    write!(out, "subvol={}", op.subvol.get());
    write!(out, " inum={}", op.inum.get());
    write!(out, " new_i_size={}", op.new_i_size.get());
}

fn truncate_set_isize(t: &TransAttempt<'_, '_>, inum: c::subvol_inum, new_i_size: u64, warn: bool)
    -> Result<(), BchError>
{
    let mut iter = BtreeIter::uninit();
    let mut inode_u = inode::__peek(t, &mut iter, inum, BtreeIterFlags::INTENT,
                                    warn.then_some(c"truncate_set_isize"))?;

    inode_u.bi_size = new_i_size;
    inode::write(t, &mut iter, &mut inode_u)
}

/// Errors are reported when there's @i_sectors_delta: not on resume.
fn __resume_truncate(
    trans:           &BtreeTrans<'_>,
    op:              &BkeyI<c::LoggedOpTruncate>,
    i_sectors_delta: Option<&mut i64>,
) -> Result<(), BchError> {
    let fs = trans.fs();
    let inum = c::subvol_inum { subvol: op.v.subvol.get() as u64, inum: op.v.inum.get() };
    let new_i_size = op.v.new_i_size.get();
    let warn_errors = i_sectors_delta.is_some();

    let ret = (|| {
        commit_do(trans, None, CommitFlags::NO_ENOSPC,
                  |t| truncate_set_isize(t, inum, new_i_size, warn_errors))?;

        // An i_size within a block of U64_MAX has nothing past its block
        let from = new_i_size.checked_next_multiple_of(fs.block_bytes()).unwrap_or(u64::MAX) >> 9;
        let mut fpunch_iter = BtreeIter::new(trans, c::btree_id::extents, pos(inum.inum, from),
                                             BtreeIterFlags::INTENT);

        match fpunch_at(trans, &mut fpunch_iter, inum, u64::MAX, i_sectors_delta) {
            Err(e) if is_restart(&e) => Ok(()),
            ret                      => ret,
        }
    })();

    if warn_errors {
        bch_err_fn!(fs, ret)
    } else {
        ret
    }
}

/// For recovery's logged ops table.
pub fn resume_truncate(trans: &BtreeTrans<'_>, op_k: &mut BkeyI) -> Result<(), BchError> {
    __resume_truncate(trans, op_k.arm::<c::LoggedOpTruncate>().expect("a truncate op"), None)
}

/// Set @inum's i_size to @new_i_size and delete its extents past that block.
pub fn truncate(fs: &Fs, inum: c::subvol_inum, new_i_size: u64, i_sectors_delta: &mut i64)
    -> Result<(), BchError>
{
    let mut op = BkeyI::<c::LoggedOpTruncate>::new();
    op.v.subvol     = (inum.subvol as u32).into();
    op.v.inum       = inum.inum.into();
    op.v.new_i_size = new_i_size.into();

    let _create_lock = fs.snapshots.create_lock.read();
    let trans = crate::btree_trans!(fs);

    commit_do(&trans, None, CommitFlags::NO_ENOSPC, |t| logged_ops::start(t, op.as_mut()))?;
    let ret = __resume_truncate(&trans, &op, Some(i_sectors_delta));
    logged_ops::finish(&trans, op.as_ref(), ret, CommitOpts::new())
}

/// C's i_sectors_delta is a u64 * here, where it's an s64 * everywhere else,
/// fpunch_at() included: the same delta.
///
/// # Safety
/// @i_sectors_delta is valid for the call.
#[c_export]
pub unsafe extern "C" fn bch2_truncate(
    c:               &Opaque<c::bch_fs>,
    inum:            c::subvol_inum,
    new_i_size:      u64,
    i_sectors_delta: *mut u64,
) -> c_int {
    ret_to_c(truncate(Fs::from_c(c), inum, new_i_size, unsafe { &mut *i_sectors_delta.cast() }))
}

// ── finsert/fcollapse ────────────────────────────────────────────────────

#[cold]
#[c_export]
pub fn bch2_logged_op_finsert_to_text(out: &mut Printbuf, _c: &Opaque<c::bch_fs>, k: BkeySC<'_>) {
    let op = c::LoggedOpFinsert::val_copy_pad(k).expect("a finsert op");

    write!(out, "subvol={}", op.subvol.get());
    write!(out, " inum={}", op.inum.get());
    write!(out, " dst_offset={}", op.dst_offset.get() as i64);
    write!(out, " src_offset={}", op.src_offset.get());
}

fn adjust_i_size(t: &TransAttempt<'_, '_>, inum: c::subvol_inum, offset: u64, len: i64, warn: bool)
    -> Result<(), BchError>
{
    let offset = offset << 9;
    let len    = len << 9;

    let mut iter = BtreeIter::uninit();
    let mut inode_u = inode::__peek(t, &mut iter, inum, BtreeIterFlags::INTENT,
                                    warn.then_some(c"adjust_i_size"))?;

    if len > 0 {
        if MAX_LFS_FILESIZE.wrapping_sub(inode_u.bi_size) < len as u64 {
            return Err(c::EFBIG.into());
        }

        if offset >= inode_u.bi_size {
            return t.fs().throw(bch_errcode::BCH_ERR_EINVAL_finsert_offset_past_eof);
        }
    }

    inode_u.bi_size = inode_u.bi_size.wrapping_add_signed(len);
    let now = t.fs().current_time();
    inode_u.bi_mtime = now;
    inode_u.bi_ctime = now;

    inode::write(t, &mut iter, &mut inode_u)
}

/*
 * The shift reached @k in an ancestor snapshot, where it stays - the ancestor
 * and its other descendants still see it - so moving it here is sharing it.
 * Returns the key to move, from @src_pos on.
 *
 * Data is shared by reflink, as remap does: the ancestor's extent becomes
 * indirect in place, in its own snapshot, and what moves is the reflink_p.
 * Copying the extent's pointers instead would leave two keys at different
 * offsets in the same space with no reflink_v between them: backpointers
 * collide, the space is counted twice, and a move of one leaves the other
 * pointing at freed space.
 *
 * An unwritten extent isn't reflinked: its space moves here, and the
 * ancestor's key is deleted over the range that moves - reading as zeroes
 * either way, and the extent update machinery keeps the rest visible.
 */
fn finsert_take_ancestor_extent<'a, 't>(t: &TransAttempt<'a, 't>, k: BkeySC<'_>, src_pos: c::bpos)
    -> Result<TransBkey<'a, 't>, BchError>
{
    let fs = t.fs();

    if k.extent_is_unwritten(fs) {
        let start = bpos_max(k.k.start_pos(), src_pos);
        let mut del = delete_key(t, k.k.p)?;
        del.k_mut().size = (k.k.p.offset - start.offset) as u32;

        t.insert(c::btree_id::extents, del, UpdateTriggerFlags::empty())?;
        return t.bkey_make_mut_noupdate(k);
    }

    /*
     * As remap; asked for only here, as it may set the superblock's
     * incompat version:
     */
    fs.check_set_feature(c::bch_sb_feature::BCH_FEATURE_reflink);
    let reflink_p_may_update_opts = fs.request_incompat_feature(
        c::bcachefs_metadata_version::bcachefs_metadata_version_reflink_p_may_update_opts).is_ok();

    let mut src_iter = BtreeIter::new(t, c::btree_id::extents, k.k.p,
                                      BtreeIterFlags::INTENT |
                                      BtreeIterFlags::ALL_SNAPSHOTS |
                                      BtreeIterFlags::NOT_EXTENTS);
    t.iter_traverse(&mut src_iter)?;

    /* Made a reflink_p in place: a one pointer extent's value is smaller */
    let orig = t.bkey_reassemble_room(k, k.k.u64s as usize * size_of::<u64>() + size_of::<c::ReflinkP>())?;

    t.result(unsafe {
        c::bch2_make_extent_indirect(t.raw(), src_iter.raw_mut(), orig.as_ptr(), reflink_p_may_update_opts)
    })?;

    /* @orig is now the ancestor's reflink_p, queued as its update */
    t.bkey_make_mut_noupdate(BkeySC::from(&orig))
}

/// Errors are reported when there's @i_sectors_delta: not on resume.
fn __resume_finsert(
    trans:               &BtreeTrans<'_>,
    op:                  &mut BkeyI<c::LoggedOpFinsert>,
    mut i_sectors_delta: Option<&mut i64>,
) -> Result<(), BchError> {
    use c::logged_op_finsert_state as S;

    let fs = trans.fs();
    let inum = c::subvol_inum { subvol: op.v.subvol.get() as u64, inum: op.v.inum.get() };
    let dst_offset = op.v.dst_offset.get();
    let src_offset = op.v.src_offset.get();
    let shift = dst_offset.wrapping_sub(src_offset) as i64;
    let len = shift.unsigned_abs();
    let mut shift_pos = op.v.pos.get();
    let insert = shift > 0;
    let warn_errors = i_sectors_delta.is_some();
    let src_pos = pos(inum.inum, src_offset);

    /*
     * check for missing subvolume before fpunch, as in resume we don't want
     * it to be a fatal error
     */
    lockrestart_do(trans, |t| subvolume::__get_snapshot(t, inum.subvol as u32, warn_errors))?;

    let ret = (|| {
        let mut iter = BtreeIter::new(trans, c::btree_id::extents, pos(inum.inum, 0), BtreeIterFlags::INTENT);
        let state = S(op.v.state as u32);
        if !matches!(state, S::LOGGED_OP_FINSERT_start | S::LOGGED_OP_FINSERT_shift_extents) {
            return Ok(());
        }

        if state == S::LOGGED_OP_FINSERT_start {
            op.v.state = S::LOGGED_OP_FINSERT_shift_extents.0 as u8;

            if insert {
                commit_do(trans, None, CommitFlags::NO_ENOSPC, |t| {
                    adjust_i_size(t, inum, src_offset, len as i64, warn_errors)?;
                    logged_ops::update(t, op.as_ref())
                })?;
            } else {
                iter.set_pos(pos(inum.inum, src_offset));

                match fpunch_at(trans, &mut iter, inum, src_offset.wrapping_add(len),
                                i_sectors_delta.as_deref_mut()) {
                    Err(e) if !is_restart(&e) => return Err(e),
                    _                         => {}
                }

                // Unchecked, as in C: the shift below runs either way
                let _ = commit_do(trans, None, CommitFlags::NO_ENOSPC, |t| logged_ops::update(t, op.as_ref()));
            }
        }

        loop {
            let disk_res = DiskReservation::new(fs);
            let t = trans.begin();

            let ret = (|| -> Result<bool, BchError> {
                let snapshot = subvolume::__get_snapshot(&t, inum.subvol as u32, warn_errors)?;

                iter.set_snapshot(snapshot);
                iter.set_pos(spos(inum.inum, shift_pos, snapshot));

                let k = if insert {
                    iter.peek_prev_min(&t, pos(inum.inum, 0))?
                } else {
                    iter.peek_max(&t, pos(inum.inum, u64::MAX))?
                };

                let Some(k) = k.filter(|k| k.k.p.inode == inum.inum && bkey_lt(src_pos, k.k.p)) else {
                    return Ok(true);
                };

                let from_ancestor = snapshot != k.k.p.snapshot;
                let shared_data = from_ancestor && k.k.extent_is_direct_data();
                let k_start = k.k.start_pos();
                let k_snapshot = k.k.p.snapshot;

                let mut copy = if shared_data {
                    finsert_take_ancestor_extent(&t, k, src_pos)?
                } else {
                    t.bkey_make_mut_noupdate(k)?
                };

                let nr_replicas = BkeySC::from(&copy).durability_safe(fs).total as u32;

                /*
                 * Only one of these fires: either the key lives in an ancestor
                 * snapshot and we're adding a reference to it here - a
                 * reservation; shared data is a reflink_p now, or unwritten
                 * space that moved - or it's already ours and only a split adds
                 * anything.
                 */
                if from_ancestor && !shared_data {
                    disk_res.add(copy.k().size as u64, nr_replicas, c::bch_reservation_flags::empty())?;
                }

                if bkey_lt(k_start, src_pos) {
                    copy.as_mut().arm::<Extents>().and_then(|e| e.cut_front(fs, src_pos))
                        .expect("it starts before src_pos and ends after");

                    /* Splitting compressed extent? */
                    if snapshot == k_snapshot {
                        let _ = disk_res.add(copy.k().size as u64, nr_replicas,
                                             c::bch_reservation_flags::BCH_DISK_RESERVATION_NOFAIL);
                    }
                }

                let mut delete = delete_key(&t, c::bpos { snapshot, ..copy.k().p })?;
                delete.k_mut().size = copy.k().size;

                let p = &mut copy.k_mut().p;
                p.offset = p.offset.wrapping_add_signed(shift);
                p.snapshot = snapshot;

                op.v.pos = if insert { delete.k().start_offset() } else { delete.k().p.offset }.into();

                t.insert(c::btree_id::extents, delete, UpdateTriggerFlags::empty())?;
                t.insert(c::btree_id::extents, copy, UpdateTriggerFlags::empty())?;
                logged_ops::update(&t, op.as_ref())?;
                t.commit(Some(&disk_res), CommitFlags::NO_ENOSPC)?;
                Ok(false)
            })();

            match ret {
                Ok(true)                 => break,
                Ok(false)                => shift_pos = op.v.pos.get(),
                Err(e) if is_restart(&e) => {}
                Err(e)                   => return Err(e),
            }
        }

        op.v.state = S::LOGGED_OP_FINSERT_finish.0 as u8;

        /*
         * A collapse's i_size comes down last; an insert's went up first, but
         * we need an inode update to update bi_journal_seq for fsync:
         */
        let (offset, len) = if insert { (0, 0) } else { (src_offset, shift) };
        commit_do(trans, None, CommitFlags::NO_ENOSPC, |t| {
            adjust_i_size(t, inum, offset, len, warn_errors)?;
            logged_ops::update(t, op.as_ref())
        })
    })();

    if warn_errors {
        bch_err_fn!(fs, ret)
    } else {
        ret
    }
}

/// For recovery's logged ops table.
pub fn resume_finsert(trans: &BtreeTrans<'_>, op_k: &mut BkeyI) -> Result<(), BchError> {
    __resume_finsert(trans, op_k.arm::<c::LoggedOpFinsert>().expect("a finsert op"), None)
}

/// Insert @len sectors of hole into @inum at @offset, shifting what's after
/// up - or, not @insert, collapse them out, shifting it down.
pub fn fcollapse_finsert(
    fs:              &Fs,
    inum:            c::subvol_inum,
    offset:          u64,
    len:             u64,
    insert:          bool,
    i_sectors_delta: &mut i64,
) -> Result<(), BchError> {
    let shift = if insert { len as i64 } else { (len as i64).wrapping_neg() };

    let mut op = BkeyI::<c::LoggedOpFinsert>::new();
    op.v.subvol     = (inum.subvol as u32).into();
    op.v.inum       = inum.inum.into();
    op.v.dst_offset = offset.wrapping_add_signed(shift).into();
    op.v.src_offset = offset.into();
    op.v.pos        = if insert { u64::MAX } else { offset }.into();

    let _create_lock = fs.snapshots.create_lock.read();
    let trans = crate::btree_trans!(fs);

    commit_do(&trans, None, CommitFlags::NO_ENOSPC, |t| logged_ops::start(t, op.as_mut()))?;
    let ret = __resume_finsert(&trans, &mut op, Some(i_sectors_delta));
    logged_ops::finish(&trans, op.as_ref(), ret, CommitOpts::new())
}

/// # Safety
/// @i_sectors_delta is valid for the call.
#[c_export]
pub unsafe extern "C" fn bch2_fcollapse_finsert(
    c:               &Opaque<c::bch_fs>,
    inum:            c::subvol_inum,
    offset:          u64,
    len:             u64,
    insert:          bool,
    i_sectors_delta: *mut i64,
) -> c_int {
    ret_to_c(fcollapse_finsert(Fs::from_c(c), inum, offset, len, insert, unsafe { &mut *i_sectors_delta }))
}
