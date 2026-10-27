// SPDX-License-Identifier: GPL-2.0

//! The stripe update: a block's extents pointed at a new stripe - an old
//! stripe's, for the blocks the new one carries - as a logged op, resumed
//! after a crash.

use crate::alloc::backpointers::{backpointer_get_key, BackpointerPos};
use crate::alloc::buckets::DiskReservation;
use crate::btree::bkey::{pos, BkeyI, BkeySC, BkeyV, BkeyVMut, POS_MIN};
use crate::btree::bkey_buf::BkeyBuf;
use crate::btree::iter::{lockrestart_do, BtreeIter, BtreeIterFlags, BtreeTrans, CommitFlags, CommitOpts, TransAttempt,
                         UpdateTriggerFlags, Watermark};
use crate::btree::write_buffer::WbMaybeFlush;
use crate::c;
use crate::data::durability::drop_extra_durability;
use crate::data::ec::stripe::{carried_blocks, ptr_in_block, Block};
use crate::data::ptrs::{brand, BkeyVPtrs, BkeyVPtrsMut, ExtentEntry, Ptrs, PtrsMut};
use crate::errcode::{bch_errcode, BchError};
use crate::fs::Fs;
use crate::init::error::id;
use crate::inode_opts::get_io_opts;
use crate::sb::counters::events;
use crate::util::ffi::Opaque;
use crate::util::log::LogMsg;
use crate::util::vstructs::Validated;
use crate::util::Printbuf;
use crate::{bch_err, fs_inconsistent, fsck_err_flags, logged_ops, prt};
use core::mem::size_of;
use core::ops::RangeInclusive;
use cstruct_macros::c_export;

// ── A stripe block's extents ─────────────────────────────────────────────
//
// A block is a bucket, so its extents are found by the bucket's
// backpointers - or, a block on a device that's gone, by those its extents
// were re-keyed on when it went: (stripe idx << 8) | block, in
// stripe_backpointers. Walked a backpointer at a time, committing after each
// extent: a block's extents won't fit in one transaction.

/// What the walk hands each backpointer's step: the extent, at its
/// iterator - None for a backpointer that resolves to none, gone or a btree
/// node.
pub(crate) type Extent<'k, 'a> = Option<(&'k BtreeIter<'a>, BkeySC<'k>)>;

/// Where block @block's backpointers are.
fn block_bps(fs: &Fs, s: BkeySC<'_, c::Stripe<Validated>>, block: u32) -> (c::btree_id, RangeInclusive<c::bpos>) {
    let ptr = &s.v().block_ptrs()[block as usize];

    // C's bch2_dev_bkey_tryget() drops the error of scheduling the repair of
    // a missing device it reports
    match fs.dev_bkey_tryget(s.untyped(), ptr.dev().into()).unwrap_or(None) {
        Some(ca) => (c::btree_id::backpointers, BackpointerPos::bucket_range(&ca, ca.ptr_bucket_pos(ptr))),
        None => {
            let dev = s.k.p.offset << 8 | u64::from(block);
            (c::btree_id::stripe_backpointers, pos(dev, 0)..=pos(dev, u64::MAX))
        }
    }
}

/// @f on each extent in block @block of stripe @s, found by backpointer, a
/// commit after each - and @res put before each.
pub(crate) fn block_for_each_extent<'t, F>(trans: &'t BtreeTrans<'t>, s: BkeySC<'_, c::Stripe<Validated>>, block: u32,
                                res: Option<&DiskReservation<'_>>, flags: CommitOpts, mut f: F)
    -> Result<(), BchError>
where
    F: for<'a, 'k, 'e> FnMut(&'k TransAttempt<'a, 't>, BkeySC<'k, c::Backpointer>, Extent<'e, 'a>) -> Result<(), BchError>,
{
    let fs = trans.fs();
    let (btree, range) = block_bps(fs, s, block);
    let mut last_flushed = WbMaybeFlush::new()?;

    BtreeIter::new(trans, btree, *range.start(), BtreeIterFlags::empty())
        .for_each_max_commit(trans, *range.end(), BtreeIterFlags::empty(), res, flags, |t, _, bp_k| {
            if let Some(res) = res {
                res.put();
            }
            let Some(bp) = bp_k.arm::<c::Backpointer>() else { return Ok(()) };

            // the stripe's own
            if bp.v().target().btree == c::btree_id::stripes {
                return Ok(());
            }
            // a btree node - not erasure coded, so the caller decides how bad
            if bp.v().level != 0 {
                return f(t, bp, None);
            }

            last_flushed.inc();
            let mut iter = BtreeIter::uninit();
            let k = backpointer_get_key(t, bp, &mut iter, BtreeIterFlags::INTENT, last_flushed.borrow())?;
            f(t, bp, k)
        })
}

// ── Pointing a block's extents at a new stripe ──

/// What a block's update did, as backpointers and their sectors.
#[derive(Default)]
struct UpdateStats {
    bp_to_deleted: (u32, u32),
    no_match:      (u32, u32),
    cached:        (u32, u32),
    done:          (u32, u32),
}

fn count(c: &mut (u32, u32), bp: BkeySC<'_, c::Backpointer>) {
    c.0 += 1;
    c.1 += bp.v().bucket_len;
}

/// Block @old, to be block @new - of the same stripe, for a new one's own
/// blocks.
struct BlockUpdate<'o, 'n> {
    old:   Block<'o>,
    new:   Block<'n>,
    stats: UpdateStats,
}

/// Point an extent in a block of a reused stripe at the new one: its stripe
/// pointer dropped, its pointer moved to the new block and given the new
/// stripe's, then the replicas that's made extra dropped - the new pointer
/// kept.
fn stripe_update_extent(t: &TransAttempt<'_, '_>, u: &mut BlockUpdate<'_, '_>, bp: BkeySC<'_, c::Backpointer>,
                        ext: Extent<'_, '_>) -> Result<(), BchError> {
    let fs = t.fs();
    let (old, new) = (u.old.stripe(), u.new.stripe());
    let (old_idx, new_idx) = (old.k.p.offset, new.k.p.offset);
    let (old_block, new_block) = (u.old.ptr(), u.new.ptr());
    let (old_dev, new_dev) = (u32::from(old_block.dev()), u32::from(new_block.dev()));

    if bp.v().level != 0 {
        fs_inconsistent!(fs, "found btree node in erasure coded bucket:\n{}", bp.untyped().to_text(fs));
        return fs.throw(bch_errcode::BCH_ERR_erasure_coding_found_btree_node);
    }

    let Some((iter, k)) = ext else {
        // the extent's gone - flushing the write buffer and looking again
        // would say so for sure, but there's no need
        count(&mut u.stats.bp_to_deleted, bp);
        fs.event_inc_trace(events::stripe_update_extent_fail, |out| {
            prt!(out, "backpointer race\n{}\n{}", bp.untyped().to_text(fs), new.untyped().to_text(fs));
        });
        return Ok(());
    };

    let p = k.arm::<Ptrs>().and_then(|kp| kp.has_device_decode(fs, old_dev).map(|d| d.p))
        .filter(|p| ptr_in_block(&old_block, &p.ptr, old.v().sectors.get().into()));
    let Some(p) = p else {
        count(&mut u.stats.no_match, bp);
        fs.event_inc_trace(events::stripe_update_extent_fail, |out| {
            prt!(out, "block {}: nomatch\n{}\n{}\n{}", u.new.idx(), bp.untyped().to_text(fs), k.to_text(fs),
                 new.untyped().to_text(fs));
        });
        return Ok(());
    };

    if p.has_ec {
        if p.ec.idx() == new_idx {
            return Ok(());
        }
        // Only carrying an extent of the old stripe's has a stripe pointer to
        // drop: the new stripe's own blocks - old_idx == new_idx - have none
        if p.ec.idx() != old_idx {
            fsck_err_flags!(t, POS_MIN, c::bch_fsck_flags::FSCK_CAN_IGNORE, id::stripe_update_stale_stripe_ptr,
                            "dropping stale stripe pointer (idx {}) while updating extent\n{}",
                            p.ec.idx(), k.to_text(fs))?;
        }
    }

    if p.ptr.cached() != 0 {
        assert!(!p.has_ec, "a cached pointer with a stripe");
        count(&mut u.stats.cached, bp);
        fs.event_inc_trace(events::stripe_update_extent_fail, |out| {
            prt!(out, "block {}: cached pointer\n{}\n{}", u.new.idx(), k.to_text(fs), new.untyped().to_text(fs));
        });
        return Ok(());
    }

    // C reads them from the edited key: its position and type are @k's,
    // which is all they're found by
    let opts = get_io_opts(t, None, k)?;
    let mut n = t.bkey_reassemble_room(k, c::BKEY_EXTENT_U64s_MAX * size_of::<u64>())?;
    {
        let mut kp = n.reborrow().arm::<PtrsMut>().expect("a key with pointers, as it had one to the block");

        if p.has_ec {
            // By (idx, block), not idx alone: an extent can have two of its
            // blocks in one stripe, and the stripe pointer to drop is the
            // moved pointer's - dropping its sibling's would leave the moved
            // one two, which fails validate as a redundant stripe entry
            let at = kp.extent_entries_at(fs).find_map(|(at, e)| match e {
                ExtentEntry::stripe_ptr(sp) if sp.idx() == p.ec.idx() && sp.block() == p.ec.block() => Some(at),
                _ => None,
            });
            if let Some(at) = at {
                kp.drop_entry(fs, at);
            }
        }

        if old_dev != new_dev {
            kp.drop_device_noerror(fs, new_dev);
        }

        let ptr = kp.ptrs_mut(fs).find(|ptr| u32::from(ptr.dev()) == old_dev).expect("the pointer to the old block");
        ptr.set_dev(new_block.dev());
        ptr.set_offset(ptr.offset() - old_block.offset() + new_block.offset());
        ptr.set_generation(new_block.generation());

        let at = kp.ptrs_decoded(fs).find(|d| u32::from(d.ptr.dev()) == new_dev).map(|d| d.pos)
            .expect("the pointer just moved");
        let stripe_ptr = c::bch_extent_stripe_ptr::new()
            .with_block(u.new.idx())
            .with_redundancy(new.v().nr_redundant)
            .with_idx(new_idx);
        kp.insert_entry(at, &[stripe_ptr.into_bits()]).expect("room: the copy's made with an extent's");

        // the replicas over data_replicas dropped, keeping the one just moved
        brand(kp.as_s(), |k| {
            let moved = k.where_(fs, |d| u32::from(d.ptr.dev()) == new_dev);
            let others = k.except(fs, moved);
            drop_extra_durability(t, &opts, k, others, true).map(drop)
        })?;
    }

    n.set_needs_reconcile(t, None, &opts, c::set_needs_reconcile_ctx::SET_NEEDS_RECONCILE_other, 0)?;
    t.update(iter, &n, UpdateTriggerFlags::SET_NEEDS_RECONCILE_DONE)?;

    count(&mut u.stats.done, bp);
    fs.event_inc_trace(events::stripe_update_extent, |out| prt!(out, "{}", n.as_sc().to_text(fs)));
    Ok(())
}

/// Every extent of block @old pointed at block @new.
fn stripe_update_bucket(trans: &BtreeTrans<'_>, old: Block<'_>, new: Block<'_>, watermark: Watermark)
    -> Result<(), BchError>
{
    let fs = trans.fs();
    let mut u = BlockUpdate { old, new, stats: UpdateStats::default() };
    let res = DiskReservation::new(fs);

    block_for_each_extent(trans, old.stripe(), old.idx().into(), Some(&res),
                          CommitOpts::new().watermark(watermark).flags(CommitFlags::NO_CHECK_RW | CommitFlags::NO_ENOSPC),
                          |t, bp, ext| stripe_update_extent(t, &mut u, bp, ext))?;

    let s = &u.stats;
    fs.event_inc_trace(events::stripe_update_bucket, |out| {
        prt!(out, "Updating block {}\n{}\n", new.idx(), new.stripe().untyped().to_text(fs));
        prt!(out, "bp_to_deleted:\t{} {}\n", s.bp_to_deleted.0, s.bp_to_deleted.1);
        prt!(out, "no_match:\t{} {}\n", s.no_match.0, s.no_match.1);
        prt!(out, "cached:\t{} {}\n", s.cached.0, s.cached.1);
        prt!(out, "done:\t{} {}\n", s.done.0, s.done.1);
    });
    Ok(())
}

/// logged_op_fail_next=stripe_update stops a reuse after its carried blocks,
/// when the old stripe is empty and recovery may delete it before the op
/// resumes. A running filesystem would reuse the half-built new stripe at
/// once, so stand in for a crash: flush the journal and go read-only, as a
/// log-flush shutdown does.
fn stripe_update_inject_crash(trans: &BtreeTrans<'_>) -> Result<(), BchError> {
    let fs = trans.fs();
    let ret = logged_ops::inject_fail(fs, c::bch_bkey_type::KEY_TYPE_logged_op_stripe_update);

    if ret.is_err() {
        trans.unlock();
        let _ = fs.journal_flush();

        let mut msg = LogMsg::new(fs);
        // printed once, when we go emergency read-only
        msg.set_suppressed(true);
        write!(msg, "logged_op_fail_next: stripe update stopped after its carried blocks\n");
        fs.emergency_read_only(&mut msg);
    }
    ret
}

/// Each data block's extents pointed at @new: the first @old_block_map.len()
/// carried from @old - block old_block_map[j] there - the rest @new's own. A
/// map that doesn't fit the two - a damaged op, resumed - is an error.
pub(crate) fn stripe_update_extents<'k>(trans: &BtreeTrans<'_>, old: BkeySC<'k, c::Stripe<Validated>>,
                                        new: BkeySC<'k, c::Stripe<Validated>>, old_block_map: &[u8],
                                        watermark: Watermark)
    -> Result<(), BchError>
{
    let fs = trans.fs();
    let carried = carried_blocks(old_block_map, old, new).or_else(|why| {
        bch_err!(fs, "stripe update: block map {old_block_map:?}: {why}\nold: {}\nnew: {}",
                 old.untyped().to_text(fs), new.untyped().to_text(fs));
        fs.throw(bch_errcode::BCH_ERR_erasure_coding_stripe_update_err)
    })?;

    trans.write_buffer_flush_sync()?;

    for (j, b) in new.data_blocks().enumerate() {
        if j != 0 && j == carried.len() {
            stripe_update_inject_crash(trans)?;
        }
        stripe_update_bucket(trans, carried.get(j).copied().unwrap_or(b), b, watermark)?;
    }
    Ok(())
}

/// Making a stripe completes space already taken, and holds open buckets
/// and writes until it's done: it can't wait behind the journal's free space
/// throttle (dev_starved_watermark()), or a full filesystem deadlocks.
pub(crate) fn create_watermark(w: c::bch_watermark) -> c::bch_watermark {
    use c::bch_watermark as W;
    if w as u32 > W::BCH_WATERMARK_copygc as u32 { w } else { W::BCH_WATERMARK_copygc }
}

// ── The logged op ────────────────────────────────────────────────────────

/// A stripe update logged op, after a crash.
pub fn resume_logged_op(trans: &BtreeTrans<'_>, op_k: &mut BkeyI) -> Result<(), BchError> {
    let fs = trans.fs();
    let op = op_k.arm::<c::LoggedOpStripeUpdate>().expect("a stripe update op");
    let (old_idx, new_idx) = (op.v.old_idx.get(), op.v.new_idx.get());

    let get = |idx| lockrestart_do(trans, |t| {
        let mut iter = BtreeIter::new(t, c::btree_id::stripes, pos(0, idx), BtreeIterFlags::CACHED);
        let k = iter.peek_slot(t)?.expect("a slot always has a key");
        Ok(k.arm::<c::Stripe>().is_some().then(|| BkeyBuf::from_key(k)))
    });

    let Some(new) = get(new_idx)? else {
        bch_err!(fs, "logged op stripe update: new stripe {new_idx} missing");
        return Ok(());
    };

    let mut old_blocks_nr = usize::from(op.v.old_blocks_nr);

    // the old stripe may be the new one, or gone
    let old = match (old_idx != 0 && old_idx != new_idx).then(|| get(old_idx)).transpose()? {
        Some(Some(old)) => old,
        Some(None) => {
            // Deleted once empty: the carried blocks' extents are re-keyed
            // already, and old_block_map doesn't index the new stripe - each
            // block's re-keyed against itself, as for a new stripe
            old_blocks_nr = 0;
            BkeyBuf::from_key(BkeySC::from(&new))
        }
        None => BkeyBuf::from_key(BkeySC::from(&new)),
    };

    fn stripe(b: &BkeyBuf) -> BkeySC<'_, c::Stripe<Validated>> {
        BkeySC::from(b).arm::<c::Stripe>().expect("a stripe, as read")
    }
    let map = &op.v.old_block_map[..old_blocks_nr.min(op.v.old_block_map.len())];
    stripe_update_extents(trans, stripe(&old), stripe(&new), map, create_watermark(c::bch_watermark::BCH_WATERMARK_normal).into())
}

#[cold]
#[c_export]
pub fn bch2_logged_op_stripe_update_to_text(out: &mut Printbuf, _c: &Opaque<c::bch_fs>, k: BkeySC<'_>) {
    let k = k.arm::<c::LoggedOpStripeUpdate>().expect("a stripe update op");
    let op = k.v();

    prt!(out, "old_idx={}", op.old_idx.get());
    prt!(out, " new_idx={}", op.new_idx.get());
    prt!(out, " old_blocks_nr={}", op.old_blocks_nr);

    let nr = usize::from(op.old_blocks_nr).min(op.old_block_map.len());
    prt!(out, " old_block_map=");
    for (i, b) in op.old_block_map[..nr].iter().enumerate() {
        prt!(out, "{}{}", if i != 0 { "," } else { "" }, b);
    }
}
