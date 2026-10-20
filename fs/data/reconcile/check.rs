// SPDX-License-Identifier: GPL-2.0

//! Reconcile's check pass, check_reconcile_work - check.c's:
//!
//! - the work btrees against what the data btrees' keys say they need: the
//!   data btree and the three work btrees walked side by side, in the work
//!   btrees' positions - data_to_rb_work_pos() - with the keys' reconcile
//!   options brought up to date on the way
//! - the reconcile phys bits, a backpointer's cache of its extent's work,
//!   against the extent - from the extent's side there - and the phys btrees
//!   against the backpointers' bits, walked side by side the same way
//! - btree node pointers' reconcile backpointers, both ways
//! - stripes' can_widen against the pool they'd widen into
//!
//! A walk side by side is peek_parallel()'s: an iterator that's ahead has no
//! key at the walk's position, where C overwrote its iter->k with a deleted
//! one.

use crate::alloc::backpointers::{backpointer_get_key, backpointers_match, bp_reconcile_phys, extent_ptr_to_bp};
use crate::alloc::buckets::DiskReservation;
use crate::alloc::check_backpointers::check_bp_exists;
use crate::btree::bkey::{bpos_ge, pos, BkeyI, BkeySC, BkeyV, POS_MIN};
use crate::btree::iter::{commit_do, lockrestart_do, peek_parallel, BtreeIter, BtreeIterFlags, BtreeNodeIter,
                         BtreeTrans, CommitFlags, TransAttempt, UpdateTriggerFlags};
use crate::btree::write_buffer::WbMaybeFlush;
use crate::c;
use crate::data::ec::create::WidenCache;
use crate::data::ec::trigger::{stripe_header_mut, stripe_widen_target_nr_data, stripe_widen_value};
use crate::data::ptrs::{BkeyVPtrs, Ptrs, PtrsMut};
use crate::data::reconcile::needs_reconcile::update_reconcile_opts;
use crate::data::reconcile::trigger::{add_reconcile_bp, data_to_rb_work_pos, rb_bp, rb_work_to_data_pos,
                                      BkeyVReconcile};
use crate::errcode::{ret_to_c, BchError};
use crate::fs::Fs;
use crate::init::error::id;
use crate::inode_opts::{get_io_opts, PerSnapshotIoOpts};
use crate::util::ffi::Opaque;
use crate::util::log::{LogLevel, LogMsg};
use crate::util::printbuf::Msg;
use crate::util::vstructs::plain_bytes;
use crate::util::Printbuf;
use crate::{fsck_err, fsck_err_on, prt, SPOS_MAX};
use core::ffi::c_int;
use core::mem::size_of;
use cstruct_macros::c_export;

const NO_ENOSPC: CommitFlags = CommitFlags::NO_ENOSPC;

/// The work btree a walk position's bit is in, and its key there if it has one.
type WorkKey<'k, 't> = (&'k BtreeIter<'t>, Option<BkeySC<'k>>);

/// Which of @keys' btrees have a key at the walk's position, a bit per btree.
fn btrees_set(keys: &[WorkKey<'_, '_>]) -> u64 {
    keys.iter().filter(|(_, k)| k.is_some()).fold(0, |m, (iter, _)| m | 1 << iter.btree().0)
}

/// What btree @want and set mask @set say, for a mismatch's message.
fn btrees_to_text(out: &mut Printbuf, want: Option<c::btree_id>, set: u64) {
    match want {
        Some(b) => write!(out, "{b}"),
        None    => write!(out, "(none)"),
    }
    write!(out, "\nbut set in: ");
    // SAFETY: C's NULL-terminated table of the btree names
    unsafe { out.prt_bitflags(c::__bch2_btree_ids.as_ptr(), set) };
}

/// The bit at @pos in work btree @iter's made whether it's @want's:
/// fix_reconcile_work_btree().
fn fix_work_btree(t: &TransAttempt<'_, '_>, want: Option<c::btree_id>, pos: c::bpos,
                  (iter, k): WorkKey<'_, '_>) -> Result<(), BchError> {
    let should = Some(iter.btree()) == want;
    let have = k.is_some_and(|k| k.k.key_type() == c::bch_bkey_type::KEY_TYPE_set);

    if should != have {
        t.bit_mod_buffered(iter.btree(), pos, should)?;
    }
    Ok(())
}

/// A backpointer's reconcile phys bits mirror its extent's work, when the
/// pointer's on a rotational device - see extent_ptr_to_bp() - set when the
/// backpointer is made, so they go stale if the device's rotational flag
/// changes while the work's pending. @bp's made @want, its phys btree entries
/// moved with them: backpointer_set_reconcile_phys().
fn backpointer_set_reconcile_phys(t: &TransAttempt<'_, '_>, bp: BkeySC<'_, c::Backpointer>,
                                  want: c::reconcile_work_id) -> Result<(), BchError> {
    let have = bp.v().reconcile_work_phys();

    let mut n = t.bkey_make_mut_noupdate(bp)?;
    n.v_mut().set_reconcile_work_phys(want);

    if let Some(btree) = have.phys_btree() {
        t.bit_mod_buffered(btree, bp.k.p, false)?;
    }
    if let Some(btree) = want.phys_btree() {
        t.bit_mod_buffered(btree, bp.k.p, true)?;
    }
    t.update_buffered(c::btree_id::backpointers, &n)
}

/// @bp's reconcile phys bits, not @want - for @extent, if it's found -
/// reported, and fixed: whether they were. reconcile_phys_wrong().
fn reconcile_phys_wrong(t: &TransAttempt<'_, '_>, bp: BkeySC<'_, c::Backpointer>, want: c::reconcile_work_id,
                        extent: Option<BkeySC<'_>>) -> Result<bool, BchError> {
    let fs = t.fs();
    let for_extent = Msg(|out: &mut Printbuf| if let Some(e) = extent {
        prt!(out, "\nfor {}", e.to_text(fs));
    });

    if fsck_err!(t, id::backpointer_reconcile_phys_wrong, "backpointer reconcile phys should be {}, is {}\n{}{}",
                 want, bp.v().reconcile_work_phys(), bp.untyped().to_text(fs), for_extent)? {
        backpointer_set_reconcile_phys(t, bp, want)?;
        return Ok(true);
    }
    Ok(false)
}

/// The "should be set, isn't" direction: only an extent knows its work, and
/// only extents with work past pending have phys bits to check - so this
/// costs a backpointer lookup per pointer, but only on those:
/// check_extent_reconcile_phys().
fn check_extent_reconcile_phys(t: &TransAttempt<'_, '_>, btree: c::btree_id, k: BkeySC<'_>,
                               last_flushed: &mut WbMaybeFlush) -> Result<(), BchError> {
    let fs = t.fs();
    let Some(kp) = k.arm::<Ptrs>() else { return Ok(()) };

    for d in kp.ptrs_decoded(fs) {
        if u32::from(d.p.ptr.dev()) == c::BCH_SB_MEMBER_INVALID {
            continue;
        }

        let mut want = extent_ptr_to_bp(fs, btree, 0, k, &d);

        let mut bp_iter = BtreeIter::new(t, want.v.btree(), want.k.p, BtreeIterFlags::empty());
        let bp_k = bp_iter.peek_slot(t)?.expect("a slot");

        // Someone else's, or stale: repaired as check_extents_to_backpointers
        // would - that only looks where a bucket's sector counts don't add
        // up, and a stale backpointer's can. Missing too: replacing a stale
        // one deletes it, committing, and a restart loses the insert.
        let Some(bp) = bp_k.arm::<c::Backpointer>().filter(|bp| backpointers_match(bp.v(), &want.v)) else {
            check_bp_exists(t, k, &mut want, last_flushed.borrow())?;
            continue;
        };

        let want_w = want.v.reconcile_work_phys();
        if bp.v().reconcile_work_phys() == want_w {
            continue;
        }

        last_flushed.maybe_flush(t, k)?;
        reconcile_phys_wrong(t, bp, want_w, Some(k))?;
    }
    Ok(())
}

/// One step of the walk of data btree @data_iter's against the work btrees
/// @rb - work, hipri, pending - from @cur_pos, a work btree position, left
/// where the step was: check_reconcile_work_one().
fn check_reconcile_work_one<'t>(t: &TransAttempt<'_, 't>, data_iter: &mut BtreeIter<'t>, rb: &mut [BtreeIter<'t>; 3],
                            s: &mut PerSnapshotIoOpts, last_flushed: &mut WbMaybeFlush, cur_pos: &mut c::bpos)
    -> Result<(), BchError>
{
    let fs = t.fs();
    let btree = data_iter.btree();
    let data_pos = rb_work_to_data_pos(*cur_pos).pos;

    data_iter.set_pos(data_pos);
    for iter in rb.iter_mut() {
        iter.set_pos(*cur_pos);
    }

    let [rb_w, rb_h, rb_p] = rb;
    if rb_w.peek(t)?.is_some_and(|k| k.k.key_type() == c::bch_bkey_type::KEY_TYPE_cookie) {
        t.delete_at(rb_w, UpdateTriggerFlags::empty())?;
        t.commit_lazy(NO_ENOSPC)?;
    }

    // what the data btree has at the walk's position, if it's ahead: for the
    // write buffer flush to remember as flushed for
    let mut deleted = BkeyI::<c::Deleted>::new();
    deleted.k.p = data_pos;

    let (pos, [(data_iter, data_k), work @ ..]) = peek_parallel(t, [data_iter, rb_w, rb_h, rb_p], |i, iter| {
        if i == 0 { data_to_rb_work_pos(btree, iter.pos()) } else { iter.pos() }
    })?;

    *cur_pos = pos;
    if bpos_ge(pos, data_to_rb_work_pos(btree, SPOS_MAX)) {
        return Ok(());
    }

    let work_id = data_k.map_or(c::reconcile_work_id::RECONCILE_WORK_none, |k| k.reconcile_work_id(fs));
    let want = work_id.btree();
    let set = btrees_set(&work);

    if set != want.map_or(0, |b| 1 << b.0) {
        let shown = match data_k { Some(k) => k, None => BkeySC::<c::Deleted>::from(&deleted).untyped() };
        last_flushed.maybe_flush(t, shown)?;

        if fsck_err!(t, id::reconcile_work_incorrectly_set, "extent should be set in {}\n{}: {}",
                     Msg(|out: &mut Printbuf| btrees_to_text(out, want, set)), btree, shown.to_text(fs))? {
            for k in work {
                fix_work_btree(t, want, pos, k)?;
            }
        }
    }

    let Some(data_k) = data_k else { return Ok(()) };

    if work_id.phys() != c::reconcile_work_id::RECONCILE_WORK_none {
        check_extent_reconcile_phys(t, btree, data_k, last_flushed)?;
    }

    let opts = get_io_opts(t, Some(&mut **s), data_k)?;
    update_reconcile_opts(t, Some(&mut **s), &opts, data_iter, 0, data_k,
                          c::set_needs_reconcile_ctx::SET_NEEDS_RECONCILE_other)
}

/// Data btree @btree against the work btrees: check_reconcile_work_data_btree().
fn check_reconcile_work_data_btree<'t>(trans: &'t BtreeTrans<'t>, btree: c::btree_id, rb: &mut [BtreeIter<'t>; 3],
                                   s: &mut PerSnapshotIoOpts, last_flushed: &mut WbMaybeFlush)
    -> Result<(), BchError>
{
    let fs = trans.fs();
    let res = DiskReservation::new(fs);
    let mut data_iter = BtreeIter::new(trans, btree, POS_MIN, BtreeIterFlags::PREFETCH | BtreeIterFlags::ALL_SNAPSHOTS);
    let mut cur_pos = data_to_rb_work_pos(btree, POS_MIN);
    let end = data_to_rb_work_pos(btree, SPOS_MAX);
    let snapshots = crate::BTREE_HAS_SNAPSHOTS_MASK & 1 << btree.0 != 0;

    loop {
        res.put();

        commit_do(trans, Some(&res), NO_ENOSPC, |t| {
            fs.recovery.progress.update(t, &data_iter)?;
            check_reconcile_work_one(t, &mut data_iter, rb, s, last_flushed, &mut cur_pos)
        })?;
        if bpos_ge(cur_pos, end) {
            return Ok(());
        }

        cur_pos = if snapshots { cur_pos.successor() } else { cur_pos.nosnap_successor() };
        last_flushed.inc();
    }
}

/// The data btrees - stripes, reflink, extents, the work btrees' order -
/// against the work btrees: check_reconcile_work_data_btrees().
fn check_reconcile_work_data_btrees(trans: &BtreeTrans<'_>) -> Result<(), BchError> {
    let fs = trans.fs();
    let rb_iter = |btree| BtreeIter::new(trans, btree, POS_MIN, BtreeIterFlags::PREFETCH | BtreeIterFlags::ALL_SNAPSHOTS);
    let mut rb = [rb_iter(c::btree_id::reconcile_work),
                  rb_iter(c::btree_id::reconcile_hipri),
                  rb_iter(c::btree_id::reconcile_pending)];

    let mut s = PerSnapshotIoOpts::new(fs);
    let mut last_flushed = WbMaybeFlush::new()?;

    fs.recovery.progress.init(fs, c"check_reconcile_work_data_btrees",
                              &[c::btree_id::extents, c::btree_id::reflink], &[]);

    for btree in [c::btree_id::stripes, c::btree_id::reflink, c::btree_id::extents] {
        check_reconcile_work_data_btree(trans, btree, &mut rb, &mut s, &mut last_flushed)?;
    }
    Ok(())
}

/// One step of the walk of the backpointers against the phys btrees @phys -
/// work, hipri - from @cur_pos, left where the step was:
/// check_reconcile_work_phys_one().
fn check_reconcile_work_phys_one<'t>(t: &TransAttempt<'_, 't>, bp_iter: &mut BtreeIter<'t>,
                                 phys: &mut [BtreeIter<'t>; 2], last_flushed: &mut WbMaybeFlush,
                                 cur_pos: &mut c::bpos) -> Result<(), BchError>
{
    let fs = t.fs();

    bp_iter.set_pos(*cur_pos);
    for iter in phys.iter_mut() {
        iter.set_pos(*cur_pos);
    }

    let [r_w, r_h] = phys;
    let (pos, [(_, bp_k), work @ ..]) = peek_parallel(t, [bp_iter, r_w, r_h], |_, iter| iter.pos())?;
    *cur_pos = pos;
    // what the backpointers have at the walk's position, if ahead
    let mut deleted = BkeyI::<c::Deleted>::new();
    deleted.k.p = pos;

    let mut bp = bp_k.and_then(|k| k.arm::<c::Backpointer>());
    let mut w = bp.map_or(c::reconcile_work_id::RECONCILE_WORK_none, |bp| bp.v().reconcile_work_phys());

    // The "set, shouldn't be" direction: what the bits should be is what the
    // backpointer's extent says, computed as extent_ptr_to_bp() does - so
    // this and the data btrees' direction can't disagree. Only backpointers
    // with bits set pay for the lookup.
    if let Some(bp_v) = bp.filter(|_| w != c::reconcile_work_id::RECONCILE_WORK_none) {
        let mut iter = BtreeIter::uninit();
        match backpointer_get_key(t, bp_v, &mut iter, BtreeIterFlags::empty(), last_flushed.borrow())? {
            None => {
                // No extent: backpointer_get_key() reported the backpointer
                // and may have deleted it, committing - don't touch it again.
                // Its phys btree entry goes below.
                bp = None;
                w = c::reconcile_work_id::RECONCILE_WORK_none;
            }
            Some(k) => {
                let want = bp_reconcile_phys(fs, bp_v.v().level.into(), bp_v.k.p.inode as u32, k);
                if want != w {
                    last_flushed.maybe_flush(t, bp_v.untyped())?;
                    // the repair moved its phys btree entry too
                    if reconcile_phys_wrong(t, bp_v, want, Some(k))? {
                        return Ok(());
                    }
                }
            }
        }
    }

    let want = w.phys_btree();
    let set = btrees_set(&work);

    if set != want.map_or(0, |b| 1 << b.0) {
        let shown = match bp { Some(bp) => bp.untyped(), None => BkeySC::<c::Deleted>::from(&deleted).untyped() };
        last_flushed.maybe_flush(t, shown)?;

        if fsck_err!(t, id::reconcile_work_phys_incorrectly_set, "backpointer should be set in {}\n{}",
                     Msg(|out: &mut Printbuf| btrees_to_text(out, want, set)), shown.to_text(fs))? {
            for k in work {
                fix_work_btree(t, want, pos, k)?;
            }
        }
    }
    Ok(())
}

/// The backpointers against the phys btrees: check_reconcile_work_phys().
fn check_reconcile_work_phys(trans: &BtreeTrans<'_>) -> Result<(), BchError> {
    let fs = trans.fs();
    fs.recovery.progress.init(fs, c"check_reconcile_work_phys", &[c::btree_id::backpointers], &[]);

    let mut cur_pos = POS_MIN;
    let mut bp_iter = BtreeIter::new(trans, c::btree_id::backpointers, POS_MIN, BtreeIterFlags::PREFETCH);
    let mut phys = [BtreeIter::new(trans, c::btree_id::reconcile_work_phys, POS_MIN, BtreeIterFlags::PREFETCH),
                    BtreeIter::new(trans, c::btree_id::reconcile_hipri_phys, POS_MIN, BtreeIterFlags::PREFETCH)];
    let mut last_flushed = WbMaybeFlush::new()?;

    loop {
        commit_do(trans, None, NO_ENOSPC, |t| {
            fs.recovery.progress.update(t, &bp_iter)?;
            check_reconcile_work_phys_one(t, &mut bp_iter, &mut phys, &mut last_flushed, &mut cur_pos)
        })?;
        if cur_pos == crate::POS_MAX {
            return Ok(());
        }

        cur_pos = cur_pos.nosnap_successor();
        last_flushed.inc();
    }
}

/// btree node pointer @k, at @level in @iter's btree: its reconcile options
/// brought up to date, and its reconcile backpointer there and its, or made:
/// check_reconcile_work_btree_key().
fn check_reconcile_work_btree_key(t: &TransAttempt<'_, '_>, iter: &BtreeIter<'_>, level: u32, k: BkeySC<'_>)
    -> Result<(), BchError>
{
    let fs = t.fs();
    let btree = iter.btree();
    let room = c::BKEY_BTREE_PTR_U64s_MAX as usize * size_of::<u64>();

    let opts = get_io_opts(t, None, k)?;
    update_reconcile_opts(t, None, &opts, iter, level, k, c::set_needs_reconcile_ctx::SET_NEEDS_RECONCILE_other)?;

    let bp_pos = k.arm::<Ptrs>().map_or(pos(0, 0), |kp| kp.reconcile_bp_pos(fs));
    // @k, with its reconcile_bp entry made @idx, as the pointer to its node
    let set_bp = |idx: u64| -> Result<(), BchError> {
        let mut n = t.bkey_reassemble_room(k, room)?;
        n.reborrow().arm::<PtrsMut>().expect("a btree node pointer, whose entries can be edited")
            .set_reconcile_bp(fs, idx)
            .expect("room for a reconcile_bp entry: BKEY_BTREE_PTR_U64s_MAX");
        t.btree_node_update_key_at_pos(btree, level, n)
    };

    if fsck_err_on!(t, bp_pos.inode != 0 && bp_pos.offset == 0, id::btree_ptr_with_no_reconcile_bp,
                    "btree ptr with no reconcile \n{}", k.to_text(fs))? {
        let bp = add_reconcile_bp(t, btree, level, k, bp_pos.inode)?;
        return set_bp(bp.offset);
    }

    if fsck_err_on!(t, bp_pos.inode == 0 && bp_pos.offset != 0, id::btree_ptr_with_bad_reconcile_bp,
                    "btree ptr with bad reconcile \n{}", k.to_text(fs))? {
        return set_bp(0);
    }

    if bp_pos == POS_MIN {
        return Ok(());
    }

    let want = rb_bp(btree, level, k);
    let mut rb_iter = BtreeIter::new(t, c::btree_id::reconcile_scan, bp_pos, BtreeIterFlags::INTENT);
    let bp_k = rb_iter.peek_slot(t)?.expect("a slot");
    let is_bp = bp_k.k.key_type() == c::bch_bkey_type::KEY_TYPE_backpointer;

    if bp_k.arm::<c::Backpointer>().is_some_and(|bp| plain_bytes(bp.v()) == plain_bytes(&want)) {
        return Ok(());
    }

    fsck_err!(t, id::btree_ptr_to_bad_reconcile_bp, "btree ptr points to bad/missing reconcile bp\n{}\n{}",
              k.to_text(fs), bp_k.to_text(fs))?;

    let alloc_bp = |iter: &BtreeIter<'_>| -> Result<(), BchError> {
        let mut bp = t.bkey_alloc_typed::<c::Backpointer>()?;
        bp.k_mut().p = iter.pos();
        *bp.v_mut() = want;
        t.update(iter, &bp, UpdateTriggerFlags::empty())
    };

    if !is_bp {
        return alloc_bp(&rb_iter);
    }

    // someone else's: ours, in a slot of our work id's
    let mut slot = BtreeIter::uninit();
    t.bkey_get_empty_slot(&mut slot, c::btree_id::reconcile_scan, pos(bp_pos.inode, 1), pos(bp_pos.inode, u64::MAX))?;
    alloc_bp(&slot)?;
    set_bp(slot.pos().offset)
}

/// Every btree node pointer, by level - the root's key, then each level's
/// keys - against its reconcile backpointer: check_reconcile_work_btrees().
fn check_reconcile_work_btrees(trans: &BtreeTrans<'_>) -> Result<(), BchError> {
    let fs = trans.fs();
    let res = DiskReservation::new(fs);
    fs.recovery.progress.init_mask(fs, c"check_reconcile_work_btrees", 0, !0);

    for btree in 0..fs.btree_id_nr_alive() {
        if fs.btree_id_root(btree).is_none() {
            continue;
        }
        let btree = c::btree_id(btree);

        for level in 1..c::BTREE_MAX_DEPTH {
            lockrestart_do(trans, |t| {
                let mut iter = BtreeIter::uninit();
                let Some((iter, k)) = iter.peek_root(t, btree, level)? else { return Ok(()) };
                assert!(k.k.is_btree_ptr(), "a btree root's key that isn't a btree pointer");

                check_reconcile_work_btree_key(t, iter, level, k)?;
                t.commit(Some(&res), NO_ENOSPC)
            })?;

            let mut node_iter = BtreeNodeIter::new(trans, btree, POS_MIN, 0, level,
                                                   BtreeIterFlags::PREFETCH | BtreeIterFlags::NOT_EXTENTS |
                                                   BtreeIterFlags::ALL_SNAPSHOTS);
            node_iter.iter().for_each_commit(trans, Some(&res), NO_ENOSPC, |t, iter, k| {
                res.put();
                fs.recovery.progress.update(t, iter)?;
                check_reconcile_work_btree_key(t, iter, level, k)
            })?;
        }
    }
    Ok(())
}

/// Each btree node backpointer in reconcile_scan - by work id, hipri to
/// pending - against the node pointer it's to: check_reconcile_btree_bps(),
/// through reconcile_bp_get_key(), C's still, which reports and deletes one
/// whose pointer isn't there or isn't its.
fn check_reconcile_btree_bps(trans: &BtreeTrans<'_>) -> Result<(), BchError> {
    use c::reconcile_work_id as w;
    let fs = trans.fs();
    fs.recovery.progress.init(fs, c"check_reconcile_btree_bps", &[c::btree_id::reconcile_scan], &[]);

    let mut iter = BtreeIter::new(trans, c::btree_id::reconcile_scan, pos(w::RECONCILE_WORK_hipri.0.into(), 0),
                                  BtreeIterFlags::PREFETCH);
    iter.for_each_max(trans, pos(w::RECONCILE_WORK_pending.0.into(), u64::MAX), |t, iter, k| {
        fs.recovery.progress.update(t, iter)?;

        let Some(bp) = k.arm::<c::Backpointer>() else { return Ok(()) };
        let mut node_iter = BtreeIter::uninit();
        // SAFETY: @bp, for the call; the key returned isn't kept
        let ret = unsafe { c::reconcile_bp_get_key(t.raw(), node_iter.raw_mut(), bp.to_c()) };
        unsafe { crate::btree::iter::bkey_s_c_to_result(ret) }.map(|_| ())
    })
}

/// Stripe @k's can_widen against how many data blocks its disk label's RW
/// members would let it have now - unless a stripes scan, pending, is to
/// refresh it: check_stripe_can_widen_one(). The reconcile_scan_stripes pass
/// refreshes it when devices' RW state changes, and get_old_stripe() demotes
/// it lazily on reuse; this surfaces one stuck out of sync.
fn check_stripe_can_widen_one(t: &TransAttempt<'_, '_>, iter: &BtreeIter<'_>, k: BkeySC<'_>,
                              cache: &mut WidenCache, scan_pending: bool) -> Result<(), BchError> {
    let fs = t.fs();
    let Some(s) = k.arm::<c::Stripe>() else { return Ok(()) };
    let v = s.v();

    let nr_devs = cache.lookup(fs, v.disk_label, v.sectors.get())?;
    // unsigned, as C's - nr_redundant past nr_blocks is damage
    let nr_data = u32::from(v.nr_blocks).wrapping_sub(v.nr_redundant.into());
    let correct = stripe_widen_value(
        stripe_widen_target_nr_data(nr_devs, v.nr_redundant.into(), fs.opts().ec_max_data_blocks.into()),
        nr_data);

    if v.can_widen() == correct || scan_pending {
        return Ok(());
    }

    if fsck_err!(t, id::stripe_can_widen_wrong, "stripe can_widen={}, should be {}\n{}",
                 v.can_widen(), correct, k.to_text(fs))? {
        let mut u = t.bkey_make_mut(iter, k, UpdateTriggerFlags::empty())?;
        stripe_header_mut(&mut u).bits.set_can_widen(correct);
    }
    Ok(())
}

/// Stripes' can_widen: check_stripe_can_widen().
fn check_stripe_can_widen(trans: &BtreeTrans<'_>) -> Result<(), BchError> {
    let fs = trans.fs();
    fs.recovery.progress.init(fs, c"check_stripe_can_widen", &[c::btree_id::stripes], &[]);

    let mut cache = WidenCache::new()?;
    let scan_pending = lockrestart_do(trans, |t| {
        // SAFETY: the attempt's transaction
        let ret = unsafe {
            c::bch2_reconcile_scan_cookie_is_set(t.raw(), c::RECONCILE_SCAN_COOKIE_stripes.into())
        };
        t.result(ret).map(|()| ret > 0)
    })?;

    let mut iter = BtreeIter::new(trans, c::btree_id::stripes, POS_MIN, BtreeIterFlags::PREFETCH);
    iter.for_each_commit(trans, None, NO_ENOSPC, |t, iter, k| {
        fs.recovery.progress.update(t, iter)?;
        check_stripe_can_widen_one(t, iter, k, &mut cache, scan_pending)
    })
}

/// The check_reconcile_work recovery pass: bch2_check_reconcile_work().
fn check_reconcile_work(fs: &Fs) -> Result<(), BchError> {
    let trans = crate::btree_trans!(fs);
    check_reconcile_work_data_btrees(&trans)?;
    check_reconcile_work_phys(&trans)?;
    check_reconcile_work_btrees(&trans)?;
    check_reconcile_btree_bps(&trans)?;
    check_stripe_can_widen(&trans)
}

#[c_export]
pub fn bch2_check_reconcile_work(c: &Opaque<c::bch_fs>) -> c_int {
    ret_to_c(check_reconcile_work(Fs::from_c(c)))
}

/// Flipping device @ca's rotational flag leaves the reconcile phys bits of
/// backpointers made under the old setting stale, and deletes after it miss
/// their phys btree entries - expected, not damage: check_reconcile_work is
/// scheduled to repair them, quietly. bch2_reconcile_rotational_changed().
#[c_export]
pub fn bch2_reconcile_rotational_changed(c: &Opaque<c::bch_fs>, ca: &c::bch_dev) {
    let fs = Fs::from_c(c);
    let mut msg = LogMsg::with_level(fs, LogLevel::INFO);
    write!(msg, "{}: rotational changed, repairing backpointer reconcile phys bits\n",
           ca.name().to_str().unwrap_or("?"));

    // Silence first, in its own write: an online pass starts as soon as it's
    // scheduled, and has to see the silenced errors
    {
        let mut w = fs.sb_write();
        w.set_err_silent(id::backpointer_reconcile_phys_wrong);
        w.set_err_silent(id::reconcile_work_phys_incorrectly_set);
    }

    if let Err(e) = crate::init::passes::run_explicit(fs, &mut msg, c::bch_recovery_pass::BCH_RECOVERY_PASS_check_reconcile_work,
                                                      c::bch_run_recovery_pass_flags::from_bits_retain(0)) {
        write!(msg, "error scheduling check_reconcile_work: {e}\n");
        msg.set_level(LogLevel::ERR);
    }
}
