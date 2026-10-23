// SPDX-License-Identifier: GPL-2.0

//! fsck of the backpointer btrees (backpointers.rs): three passes, each
//! checking one direction of the index.
//!
//! - check_btree_backpointers: every backpointer is on a device, in a bucket
//!   that exists.
//! - check_extents_to_backpointers: every pointer has its backpointer. It
//!   sums each bucket's backpointers against its alloc key's sector counts,
//!   then walks every btree's pointers into the buckets that come up short,
//!   with the backpointer btree's nodes holding them pinned. The buckets are
//!   recorded in each device's bucket bitmaps, which copygc reads too.
//! - check_backpointers_to_extents: every backpointer points to something
//!   that points back - walked in target order, through BpScanIter.
//!
//! Before calling a key missing, a pass flushes the write buffer and looks
//! again - see backpointers.rs.

use crate::alloc::background::alloc_to_v4;
use crate::alloc::backpointers::{backpointer_del, backpointers_match, backpointers_maybe_flush,
                                 bucket_backpointer_mod, extent_ptr_to_bp, get_key, BackpointerPos,
                                 BpScanIter};
use crate::alloc::buckets::DiskReservation;
use crate::alloc::check_data::{bkey_drop_device_and_update, kill_replica_if_checksum_bad};
use crate::btree::bkey::prelude::*;
use crate::btree::bkey::{pos, spos, BkeyI, BkeySC, POS_MAX, POS_MIN, SPOS_MAX};
use crate::btree::bkey_buf::BkeyBuf;
use crate::btree::cache::BtreeCachePins;
use crate::btree::iter::{commit_do, lockrestart_do, BtreeIter, BtreeIterFlags, BtreeTrans, CommitFlags,
                         TransAttempt, UpdateTriggerFlags};
use crate::btree::write_buffer::{WbFlush, WbMaybeFlush};
use crate::c;
use crate::data::extents::Extents;
use crate::data::ptrs::{crc_is_compressed, extents_match, BkeyVPtrs, Ptrs};
use crate::data::reflink::make_extent_indirect;
use crate::errcode::{bch_errcode, ret_to_c, BchError};
use crate::fs::Fs;
use crate::init::error::id;
use crate::init::passes;
use crate::init::progress::Progress;
use crate::opts::prt_data_type;
use crate::types::bits_to_longs;
use crate::util::bitmap::{AtomicBitmap, AtomicULong};
use crate::util::log::{log_msg_start, print_str, LogLevel, LogMsg, Ratelimit};
use crate::util::rcu::{read_lock, ReadGuard};
use crate::util::Printbuf;
use crate::{fsck_err, inode_fsck_err, prt};
use core::ffi::c_int;
use core::ops::{ControlFlow, RangeInclusive};
use core::sync::atomic::Ordering;
use cstruct_macros::c_export;

// ── fsck: check_btree_backpointers ───────────────────────────────────────

fn check_backpointer_has_valid_bucket(t: &TransAttempt<'_, '_>, k: BkeySC<'_>,
                                      last_flushed: &mut WbMaybeFlush) -> Result<(), BchError> {
    if k.k.key_type() != c::bch_bkey_type::KEY_TYPE_backpointer {
        return Ok(());
    }

    let fs = t.fs();
    let bucket = {
        let rcu = read_lock();
        fs.dev_rcu(k.k.p.inode as u32, &rcu).map(|ca| BackpointerPos::from_bpos(fs, k.k.p).bucket(ca))
    };

    let Some(bucket) = bucket else {
        backpointers_maybe_flush(t, k, last_flushed.borrow())?;

        if fsck_err!(t, id::backpointer_to_missing_device, "backpointer for missing device:\n{}",
                     k.to_text(fs))? {
            backpointer_del(t, c::btree_id::backpointers, k.k.p)?;
        }
        return Ok(());
    };

    let mut alloc_iter = BtreeIter::new(t, c::btree_id::alloc, bucket, BtreeIterFlags::empty());
    let alloc_k = alloc_iter.peek_slot(t)?.expect("a slot always has a key");

    if alloc_k.k.key_type() != c::bch_bkey_type::KEY_TYPE_alloc_v4 {
        backpointers_maybe_flush(t, k, last_flushed.borrow())?;

        let p = alloc_iter.pos();
        if fsck_err!(t, id::backpointer_to_missing_alloc, "backpointer for nonexistent alloc key: {}:{}:0\n{}",
                     { p.inode }, { p.offset }, k.to_text(fs))? {
            backpointer_del(t, c::btree_id::backpointers, k.k.p)?;
        }
    }
    Ok(())
}

fn check_btree_backpointers(fs: &Fs) -> Result<(), BchError> {
    let progress = Progress::recovery(fs, c"bch2_check_btree_backpointers", &[c::btree_id::backpointers], &[]);
    let mut last_flushed = WbMaybeFlush::new()?;

    let trans = crate::btree_trans!(fs);
    let mut iter = BtreeIter::new(&trans, c::btree_id::backpointers, POS_MIN, BtreeIterFlags::empty());
    iter.for_each_commit(&trans, None, CommitFlags::NO_ENOSPC, |t, iter, k| {
        progress.update(t, iter)?;
        check_backpointer_has_valid_bucket(t, k, &mut last_flushed)
    })
}

crate::recovery_pass!(bch2_check_btree_backpointers => check_btree_backpointers);

// ── fsck: an extent's backpointer ────────────────────────────────────────

fn bp_missing(t: &TransAttempt<'_, '_>, extent: BkeySC<'_>, bp: &mut BkeyI<c::Backpointer>,
              bp_found: BkeySC<'_>) -> Result<(), BchError> {
    let fs = t.fs();
    let mut buf = Printbuf::new();
    prt!(&mut buf, "missing backpointer\nfor:  {}", extent.to_text(fs));
    prt!(&mut buf, "\nwant: {}", BkeySC::<c::Backpointer>::from(&*bp).to_text(fs));
    if !bp_found.k.is_deleted() {
        prt!(&mut buf, "\ngot:  {}", bp_found.to_text(fs));
    }

    if fsck_err!(t, id::ptr_to_missing_backpointer, "{}", buf)? {
        bucket_backpointer_mod(t, extent, bp, true)?;
    }
    Ok(())
}

/// A pointer to a bucket @dev doesn't have counts as stale.
fn bkey_dev_ptr_stale(fs: &Fs, k: BkeySC<'_>, dev: u32) -> bool {
    let rcu = read_lock();
    let Some(ca) = fs.dev_rcu(dev, &rcu) else { return false };
    let Some(kp) = k.arm::<Ptrs>() else { return false };

    let stale = kp.ptrs(fs).any(|p| u32::from(p.dev()) == dev && ca.ptr_stale(p, &rcu).is_none_or(|s| s != 0));
    stale
}

fn drop_double_allocation(t: &TransAttempt<'_, '_>, btree: c::btree_id, level: u8, k: BkeySC<'_>, dev: u32)
    -> Result<(), BchError>
{
    bkey_drop_device_and_update(t, btree, level.into(), k, dev,
                                c::bch_key_type_errors::KEY_TYPE_ERROR_double_allocation)
}

/// One of two extents sharing physical space, and its logical range in the overlap.
struct DupExtent<'k> {
    btree:   c::btree_id,
    level:   u8,
    k:       BkeySC<'k>,
    overlap: core::ops::Range<u64>,
}

/// Just the overlapping part of each becomes a reflink_p of one shared reflink_v;
/// the rest of each stays an extent, split off when the reflink_p is inserted.
fn extents_to_reflink<'a>(t: &TransAttempt<'a, '_>, e1: &DupExtent<'_>, e2: &DupExtent<'_>)
    -> Result<(), BchError>
{
    let fs = t.fs();
    // as C's: a u32
    let overlap_size = (e1.overlap.end - e1.overlap.start) as u32;
    assert_eq!(u64::from(overlap_size), e2.overlap.end - e2.overlap.start, "overlaps of different sizes");

    let p1 = e1.k.k.p;
    let mut iter1 = BtreeIter::new_level(t, e1.btree, p1, e1.level.into(), BtreeIterFlags::INTENT);
    t.iter_traverse(&mut iter1)?;

    // With room for the reflink_p make_extent_indirect() makes it, in place
    let mut k1_mut = t.bkey_reassemble_room(e1.k, size_of::<BkeyI<c::ReflinkP>>())?;
    {
        let mut e = k1_mut.as_mut().arm::<Extents>().expect("an extent");
        e.cut_front_in_place(fs, spos(p1.inode, e1.overlap.start, p1.snapshot));
        e.cut_back_in_place(spos(p1.inode, e1.overlap.end, p1.snapshot));
    }

    make_extent_indirect(t, &iter1, &mut k1_mut, false)?;

    let r_v_idx = k1_mut.as_ref().arm::<c::ReflinkP>().expect("a reflink_p, as just made").v.idx();

    let p2 = e2.k.k.p;
    let mut iter2 = BtreeIter::new_level(t, e2.btree, p2, e2.level.into(), BtreeIterFlags::INTENT);
    t.iter_traverse(&mut iter2)?;

    let mut rp = t.bkey_alloc_typed::<c::ReflinkP>()?;
    let k = rp.k_mut();
    k.p = spos(p2.inode, e2.overlap.end, p2.snapshot);
    k.size = overlap_size;
    k.bversion = e2.k.k.bversion;
    rp.v_mut().set_idx(r_v_idx);

    t.update(&iter2, &rp, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)
}

fn check_bp_dup(t: &TransAttempt<'_, '_>, extent: BkeySC<'_>, bp: &mut BkeyI<c::Backpointer>,
                other_bp: BkeySC<'_, c::Backpointer>) -> Result<(), BchError> {
    let fs = t.fs();
    let dev = bp.k.p.inode as u32;
    let ov = *other_bp.v();
    let other_btree = c::btree_id(ov.btree_id.into());

    let mut other_extent_iter = BtreeIter::uninit();
    let other_extent = match get_key(t, other_bp, &mut other_extent_iter, BtreeIterFlags::empty(), None, false) {
        Err(e) if e.matches(bch_errcode::BCH_ERR_backpointer_to_overwritten_btree_node) =>
            return bp_missing(t, extent, bp, other_bp.untyped()),
        Err(e)      => return Err(e),
        Ok(None)    => return bp_missing(t, extent, bp, other_bp.untyped()),
        Ok(Some(k)) => k,
    };

    if bkey_dev_ptr_stale(fs, other_extent, dev) {
        return drop_double_allocation(t, other_btree, ov.level, other_extent, dev);
    }

    if extents_match(fs, extent, other_extent) {
        let mut msg = LogMsg::new(fs);
        prt!(&mut msg, "duplicate versions of same extent, deleting smaller\n{}\n{}",
             extent.to_text(fs), other_extent.to_text(fs));

        if other_extent.k.size <= extent.k.size {
            return drop_double_allocation(t, other_btree, ov.level, other_extent, dev);
        } else {
            drop_double_allocation(t, c::btree_id(bp.v.btree_id.into()), bp.v.level, extent, dev)?;
            return bp_missing(t, extent, bp, other_bp.untyped());
        }
    }

    let btree = c::btree_id(bp.v.btree_id.into());
    if kill_replica_if_checksum_bad(t, other_btree, ov.level.into(), other_extent, btree, extent, dev)? {
        return bp_missing(t, extent, bp, other_bp.untyped());
    }
    if kill_replica_if_checksum_bad(t, btree, bp.v.level.into(), extent, other_btree, other_extent, dev)? {
        return Ok(());
    }

    // Both replicas verify clean: a physical region is referenced from two
    // distinct logical positions, possibly with partial overlap. This can
    // happen after btree topology recovery from media errors - convert the
    // overlapping portion to a reflink_v shared by two reflink_p's so neither
    // file loses data. Non-overlapping prefix/suffix of each extent stays as
    // a regular extent.
    //
    // Only valid for leaf extents; btree pointers can't be reflinked.
    let is_extent = |k: BkeySC<'_>| k.k.key_type() == c::bch_bkey_type::KEY_TYPE_extent;
    if is_extent(extent) && is_extent(other_extent) {
        let (bp_sub, other_bp_sub) = {
            let rcu = read_lock();
            let Some(ca) = fs.dev_rcu(dev, &rcu) else { return Ok(()) };
            (BackpointerPos::from_bpos(fs, bp.k.p).bucket_and_offset(ca).1,
             BackpointerPos::from_bpos(fs, other_bp.k.p).bucket_and_offset(ca).1)
        };

        // as C's: u32s
        let overlap_sub_start = bp_sub.max(other_bp_sub);
        let overlap_sub_end = bp_sub.wrapping_add(bp.v.bucket_len)
            .min(other_bp_sub.wrapping_add(ov.bucket_len));
        if overlap_sub_end <= overlap_sub_start {
            return Ok(()); // bps don't actually overlap
        }

        // The physical overlap is a logical one only for uncompressed data.
        // Compressed data can't be split: two extents share it whole, if each
        // is the same view of the same data, or can't be repaired here
        let ptr_to_dev = |k: BkeySC<'_>| k.arm::<Ptrs>().expect("an extent")
            .has_device_decode(fs, dev).expect("a pointer to the backpointer's device").p;
        let (p1, p2) = (ptr_to_dev(extent), ptr_to_dev(other_extent));
        let (k1_start, k2_start, overlap_len) = if crc_is_compressed(&p1.crc) || crc_is_compressed(&p2.crc) {
            let same_view = p1.ptr.offset() == p2.ptr.offset() &&
                p1.crc.compressed_size == p2.crc.compressed_size &&
                p1.crc.offset == p2.crc.offset &&
                p1.crc.live_size == p2.crc.live_size &&
                extent.k.size == other_extent.k.size;
            if !same_view {
                let mut msg = LogMsg::new(fs);
                prt!(&mut msg, "duplicate compressed extents pointing to overlapping space on dev {dev}, \
                                not the same data - can't share them\n{}\n{}",
                     extent.to_text(fs), other_extent.to_text(fs));
                return fs.throw(bch_errcode::BCH_ERR_fsck_repair_unimplemented);
            }
            (extent.k.start_offset(), other_extent.k.start_offset(), u64::from(extent.k.size))
        } else {
            (extent.k.start_offset() + u64::from(overlap_sub_start - bp_sub),
             other_extent.k.start_offset() + u64::from(overlap_sub_start - other_bp_sub),
             u64::from(overlap_sub_end - overlap_sub_start))
        };

        let mut buf = Printbuf::new();
        prt!(&mut buf, "duplicate extents pointing to overlapping space on dev {}, converting overlap to reflink\n{}\n{}",
             { bp.k.p.inode }, extent.to_text(fs), other_extent.to_text(fs));

        // Both files are damaged - sharing physical space means at least one
        // of them reads the other's bytes. The pos-ful report records against
        // the first inode; record the second explicitly (KEY_TYPE_extent only
        // lives in the extents btree, so both attribute):
        if inode_fsck_err!(t, extent.k.p, id::dup_extents_to_reflink, "{}", buf)? {
            t.result(unsafe { c::bch2_damage_record(t.raw(), other_extent.k.p, id::dup_extents_to_reflink) })?;
            extents_to_reflink(t,
                &DupExtent { btree, level: bp.v.level, k: extent, overlap: k1_start..k1_start + overlap_len },
                &DupExtent { btree: other_btree, level: ov.level, k: other_extent,
                             overlap: k2_start..k2_start + overlap_len })?;
        }
        return Ok(());
    }

    let mut msg = LogMsg::new(fs);
    prt!(&mut msg, "duplicate extents pointing to same space on dev {}\n{}\n{}",
         { bp.k.p.inode }, extent.to_text(fs), other_extent.to_text(fs));
    fs.throw(bch_errcode::BCH_ERR_fsck_repair_unimplemented)
}

/// Repairs as check_extents_to_backpointers does: makes a missing one, resolves
/// one that's someone else's.
pub fn check_bp_exists(t: &TransAttempt<'_, '_>, extent: BkeySC<'_>, bp: &mut BkeyI<c::Backpointer>,
                       mut last_flushed: WbFlush<'_>) -> Result<(), BchError> {
    let mut bp_iter = BtreeIter::new(t, bp.v.btree(), bp.k.p, BtreeIterFlags::empty());
    let bp_found = bp_iter.peek_slot(t)?.expect("a slot always has a key");

    match bp_found.arm::<c::Backpointer>() {
        None => {
            last_flushed.maybe_flush(t, extent)?;
            bp_missing(t, extent, bp, bp_found)
        }
        Some(found) if !backpointers_match(found.v(), &bp.v) => {
            last_flushed.maybe_flush(t, extent)?;
            check_bp_dup(t, extent, bp, found)
        }
        Some(_) => Ok(()),
    }
}

// ── fsck: check_extents_to_backpointers ──────────────────────────────────
//
// Backpointers are checked by bucket, not one by one: each bucket's, of its
// generation, are summed against its alloc key's sector counts. More than
// it says, and they're checked against what they point to; not what it
// says, and the bucket goes in ca->bucket_backpointer_mismatch - and
// _empty, if there are none - for the walk of every extent and btree node
// pointer that follows, which checks the pointers into those buckets.

/// An alloc key's sector counts, or what its backpointers add up to.
#[derive(Default, PartialEq)]
struct BucketSectors {
    dirty:  u32,
    cached: u32,
    stripe: u32,
}

impl BucketSectors {
    fn of(a: &c::AllocV4) -> Self {
        Self { dirty: a.dirty_sectors, cached: a.cached_sectors, stripe: a.stripe_sectors }
    }

    fn add(&mut self, bp: &c::Backpointer) {
        use c::bch_data_type as dt;
        let counter = match c::bch_data_type(bp.data_type.into()) {
            dt::BCH_DATA_btree | dt::BCH_DATA_user    => &mut self.dirty,
            dt::BCH_DATA_cached                       => &mut self.cached,
            dt::BCH_DATA_stripe | dt::BCH_DATA_parity => &mut self.stripe,
            _ => return,
        };
        // as C's: u32s
        *counter = counter.wrapping_add(bp.bucket_len);
    }

    fn any_over(&self, a: &Self) -> bool {
        self.dirty > a.dirty || self.cached > a.cached || self.stripe > a.stripe
    }

    fn is_empty(&self) -> bool {
        self.dirty.wrapping_add(self.stripe).wrapping_add(self.cached) == 0
    }
}

/// If the backpointers are over, they're checked against what they point to, once
/// (@checked_bad), and the attempt restarted; if off, the bucket goes in the
/// bitmaps. Returns whether it did.
fn check_bucket_backpointer_mismatch(t: &TransAttempt<'_, '_>, alloc_k: BkeySC<'_>,
                                     mut last_flushed: WbFlush<'_>, checked_bad: &mut c::bpos)
    -> Result<bool, BchError>
{
    use c::bch_data_type as dt;

    let fs = t.fs();
    let a = alloc_to_v4(alloc_k);

    if matches!(c::bch_data_type(a.data_type.into()), dt::BCH_DATA_sb | dt::BCH_DATA_journal | dt::BCH_DATA_parity) {
        return Ok(false);
    }

    let Some(ca) = fs.dev_bucket_tryget_noerror(alloc_k.k.p) else { return Ok(false) };
    let bp_range = BackpointerPos::bucket_range(&ca, alloc_k.k.p);
    let before_bucket_gen = fs.version_upgrade_complete().0 <
        c::bcachefs_metadata_version::bcachefs_metadata_version_backpointer_bucket_gen.0;

    let mut sectors = BucketSectors::default();
    let mut nr_deletes = 0u32;
    let mut need_commit = false;

    let mut iter = BtreeIter::new(t, c::btree_id::backpointers, *bp_range.start(), BtreeIterFlags::empty());
    iter.for_each_max_norestart(t, *bp_range.end(), |_, bp_k| {
        let Some(bp) = bp_k.arm::<c::Backpointer>() else { return Ok(ControlFlow::Continue(())) };
        let v = *bp.v();

        if before_bucket_gen && (v.bucket_gen != a.generation || { v.flags } != 0) {
            backpointer_del(t, c::btree_id::backpointers, bp_k.k.p)?;
            nr_deletes += 1;

            if nr_deletes > 256 {
                t.commit(None, CommitFlags::NO_ENOSPC)?;
                t.trans().write_buffer_flush_sync()?;
                return Err(t.restart(bch_errcode::BCH_ERR_transaction_restart_write_buffer_flush));
            }

            need_commit = true;
            return Ok(ControlFlow::Continue(()));
        }

        if v.bucket_gen == a.generation {
            sectors.add(&v);
        }
        Ok(ControlFlow::<()>::Continue(()))
    })?;

    if need_commit {
        t.commit(None, CommitFlags::NO_ENOSPC)?;
    }

    let have = BucketSectors::of(&a);

    if sectors.any_over(&have) {
        if *checked_bad != alloc_k.k.p {
            *checked_bad = alloc_k.k.p;
            check_bucket_backpointers_to_extents(t.trans(), &ca, alloc_k.k.p, last_flushed)?;
            return Err(t.restart(bch_errcode::BCH_ERR_transaction_restart_nested));
        }

        backpointers_maybe_flush(t, alloc_k, last_flushed.reborrow())?;

        let p = alloc_k.k.p;
        let mut msg = LogMsg::new(fs);
        prt!(&mut msg, "backpointer sectors > bucket sectors, but found no bad backpointers\n\
                        bucket {}:{} data type ", { p.inode }, { p.offset });
        prt_data_type(&mut msg, c::bch_data_type(a.data_type.into()));
        prt!(&mut msg, ", counters\n");
        if sectors.dirty > have.dirty {
            prt!(&mut msg, "dirty: {} > {}\n", sectors.dirty, have.dirty);
        }
        if sectors.cached > have.cached {
            prt!(&mut msg, "cached: {} > {}\n", sectors.cached, have.cached);
        }
        if sectors.stripe > have.stripe {
            prt!(&mut msg, "stripe: {} > {}\n", sectors.stripe, have.stripe);
        }

        // as C's: whatever of the bucket it reads
        let mut iter = BtreeIter::new(t, c::btree_id::backpointers, *bp_range.start(), BtreeIterFlags::empty());
        let _ = iter.for_each_max_norestart(t, *bp_range.end(), |_, bp_k| {
            prt!(&mut msg, "{}\n", bp_k.to_text(fs));
            Ok(ControlFlow::<()>::Continue(()))
        });
        crate::warn_on!(true);
    }

    if sectors == have {
        return Ok(false);
    }

    // Post 1.14 upgrade, we assume that backpointers are mostly correct and
    // a sector count mismatch is probably due to a write buffer race.
    //
    // Pre upgrade, we expect all the buckets to be wrong, a write buffer
    // flush is pointless:
    if !before_bucket_gen {
        if c::bch_data_type(a.data_type.into()) == dt::BCH_DATA_btree {
            t.trans().unlock_long();
            unsafe { c::bch2_btree_interior_updates_flush(fs.as_ptr()) };
        }
        backpointers_maybe_flush(t, alloc_k, last_flushed)?;
    }

    ca.bucket_backpointer_mismatch.set(&ca, alloc_k.k.p.offset)?;
    if sectors.is_empty() {
        ca.bucket_backpointer_empty.set(&ca, alloc_k.k.p.offset)?;
    }
    Ok(true)
}

/// Schedules check_extents_to_backpointers - ratelimited from copygc, until 1/128
/// of the device's buckets are short.
fn check_one_bucket_backpointer_mismatch(trans: &BtreeTrans<'_>, ca: &c::bch_dev, bucket: u64,
                                         copygc: bool, mut last_flushed: WbFlush<'_>)
    -> Result<(), BchError>
{
    let fs = ca.fs();

    // Outside the restart loop: after checking a bucket with too many
    // backpointers against their extents, check_bucket_backpointer_mismatch()
    // restarts - reset this and it does that forever.
    let mut checked_bad = POS_MAX;
    let had_mismatch = lockrestart_do(trans, |t| {
        // Not BTREE_ITER_cached: scrub calls this once per bucket for a whole
        // device (bch2_move_data_phys() fills in the gaps between
        // backpointers), and a cached iterator would leave a key cache entry
        // behind for every one of them. Uncached still sees a dirty cached key
        // - alloc is a cached btree, so bch2_btree_iter_flags() gives us
        // BTREE_ITER_with_key_cache, which consults an existing entry without
        // creating one.
        let mut alloc_iter = BtreeIter::new(t, c::btree_id::alloc, pos(ca.dev_idx.into(), bucket),
                                            BtreeIterFlags::empty());
        let k = alloc_iter.peek_slot(t)?.expect("a slot always has a key");
        check_bucket_backpointer_mismatch(t, k, last_flushed.reborrow(), &mut checked_bad)
    })?;

    if !had_mismatch {
        return Ok(());
    }

    let nr = ca.bucket_backpointer_mismatch.nr();
    let allowed = if copygc { ca.mi.nbuckets >> 7 } else { 0 };

    let mut buf = Printbuf::new();
    log_msg_start(&mut buf, ca.name());
    prt!(&mut buf, "Detected missing backpointers in bucket {bucket}, now have {nr}/{} with missing\n",
         { ca.mi.nbuckets });

    let flags = if nr < allowed {
        c::bch_run_recovery_pass_flags::RUN_RECOVERY_PASS_ratelimit
    } else {
        c::bch_run_recovery_pass_flags::from_bits_retain(0)
    };
    // as C's: the message says whether it's scheduled
    let _ = passes::run_explicit(fs, &mut buf,
                                 c::bch_recovery_pass::BCH_RECOVERY_PASS_check_extents_to_backpointers, flags);

    static RATELIMIT: Ratelimit = Ratelimit::new();
    if !RATELIMIT.suppress(fs, c"bch2_check_bucket_backpointer_mismatch") {
        print_str(fs, LogLevel::ERR, &mut buf);
    }
    Ok(())
}

/// # Safety
/// @trans isn't in an attempt; @last_flushed is the caller's.
#[c_export]
pub unsafe fn bch2_check_bucket_backpointer_mismatch<'t>(trans: &'t BtreeTrans<'t>, ca: &c::bch_dev,
                                                         bucket: u64, copygc: bool,
                                                         last_flushed: *mut c::wb_maybe_flush) -> c_int {
    let last_flushed = unsafe { WbFlush::from_raw(last_flushed) }.expect("a wb_maybe_flush");
    ret_to_c(check_one_bucket_backpointer_mismatch(trans, ca, bucket, copygc, last_flushed))
}

struct ExtentsToBpState {
    bp_range:     RangeInclusive<c::bpos>,
    last_flushed: WbMaybeFlush,
}

/// A bucket with no backpointers has them all put back without looking.
fn check_extent_to_backpointers(t: &TransAttempt<'_, '_>, s: &mut ExtentsToBpState, btree: c::btree_id,
                                level: u32, k: BkeySC<'_>) -> Result<(), BchError> {
    let fs = t.fs();
    let Some(kp) = k.arm::<Ptrs>() else { return Ok(()) };

    for d in kp.ptrs_decoded(fs) {
        let mut bp = extent_ptr_to_bp(fs, btree, level, k, &d);
        let dev = u32::from(d.p.ptr.dev());

        if dev == c::BCH_SB_MEMBER_INVALID {
            if d.p.has_ec {
                check_bp_exists(t, k, &mut bp, s.last_flushed.borrow())?;
            }
            continue;
        }

        let empty = {
            let rcu = read_lock();
            let Some(ca) = fs.dev_rcu(dev, &rcu) else { continue };

            if d.p.ptr.cached() != 0 && ca.ptr_stale(&d.p.ptr, &rcu).is_none_or(|s| s != 0) {
                continue;
            }

            let b = ca.ptr_bucket(&d.p.ptr);
            if !ca.bucket_backpointer_mismatch.test(b) {
                continue;
            }
            ca.bucket_backpointer_empty.test(b)
        };

        if !s.bp_range.contains(&bp.k.p) {
            continue;
        }

        if !empty {
            check_bp_exists(t, k, &mut bp, s.last_flushed.borrow())?;
        } else {
            bucket_backpointer_mod(t, k, &mut bp, true)?;
        }
    }
    Ok(())
}

/// The root's key is checked a level above the root. Returns the root's level.
fn check_btree_root_to_backpointers(t: &TransAttempt<'_, '_>, s: &mut ExtentsToBpState, btree: c::btree_id)
    -> Result<u32, BchError>
{
    let fs = t.fs();
    let root_level = fs.btree_id_root(btree.0).expect("a btree with a root").c.level;

    let mut iter = BtreeIter::new_level(t, btree, POS_MIN, root_level.into(), BtreeIterFlags::empty());
    let b = iter.peek_node(t)?.expect("a btree's root node");

    if !fs.btree_id_root(b.c.btree_id.into()).is_some_and(|root| core::ptr::eq(root, b)) {
        return Err(t.restart(bch_errcode::BCH_ERR_transaction_restart_lock_root_race));
    }

    let level = u32::from(b.c.level);
    check_extent_to_backpointers(t, s, btree, level + 1, BkeySC::from(b.key_i()))?;
    Ok(level)
}

fn check_extents_to_backpointers_pass(fs: &Fs, trans: &BtreeTrans<'_>, s: &mut ExtentsToBpState)
    -> Result<(), BchError>
{
    let progress = Progress::recovery_masks(fs, c"extents_to_backpointers", crate::BTREE_HAS_DATA_PTRS_MASK, !0);
    // extents btree updates always require a disk res passed in
    let res = DiskReservation::new(fs);

    for btree in (0..fs.btree_id_nr_alive()).map(c::btree_id) {
        let depth = if btree.has_data_ptrs() { 0 } else { 1 };

        let mut root_level = 0;
        commit_do(trans, Some(&res), CommitFlags::NO_ENOSPC, |t| {
            root_level = check_btree_root_to_backpointers(t, s, btree)?;
            Ok(())
        })?;

        for level in (depth..=root_level).rev() {
            let mut iter = BtreeIter::new_level(trans, btree, POS_MIN, level, BtreeIterFlags::PREFETCH);
            iter.for_each_commit(trans, Some(&res), CommitFlags::NO_ENOSPC, |t, iter, k| {
                progress.update(t, iter)?;
                s.last_flushed.inc();
                check_extent_to_backpointers(t, s, btree, level, k)
            })?;
        }
    }
    Ok(())
}

/// Buckets with none don't count - theirs are recreated without looking. A
/// btree_ptr has no min_key, so might.
fn backpointer_node_has_missing(fs: &Fs, k: BkeySC<'_>) -> bool {
    use c::bch_bkey_type as t;

    match k.k.key_type() {
        t::KEY_TYPE_btree_ptr_v2 => {
            let rcu = read_lock();
            let mut p = k.arm::<c::BtreePtrV2>().expect("a btree_ptr_v2").v().min_key;

            while p.inode <= k.k.p.inode && p.inode < u64::from(fs.sb.nr_devices.get()) {
                if let Some(ca) = fs.dev_rcu(p.inode as u32, &rcu) {
                    let bucket = BackpointerPos::from_bpos(fs, p).bucket(ca);
                    let nbuckets = ca.mi.nbuckets;
                    let mismatch = ca.bucket_backpointer_mismatch.words(&rcu);
                    let empty = ca.bucket_backpointer_empty.words(&rcu);

                    let next = AtomicBitmap::from_words(mismatch)
                        .next_andnot(AtomicBitmap::from_words(empty),
                                     bucket.offset.min(nbuckets) as usize, nbuckets as usize);

                    if let Some(next) = next {
                        let next = pos(bucket.inode, next as u64);
                        if *BackpointerPos::bucket_range(ca, next).end() <= k.k.p {
                            return true;
                        }
                    }
                }
                p = spos(p.inode + 1, 0, 0);
            }
            false
        }
        t::KEY_TYPE_btree_ptr => true,
        _ => false,
    }
}

fn btree_node_get_and_pin(t: &TransAttempt<'_, '_>, pins: &BtreeCachePins<'_>, k: &BkeyI, btree: c::btree_id,
                          level: u32) -> Result<(), BchError> {
    let mut iter = BtreeIter::new_level(t, btree, k.k.p, level, BtreeIterFlags::empty());
    if let Some(b) = iter.peek_node(t)? {
        pins.node(b);
    }
    Ok(())
}

/// All prefetched first, so they're read in parallel. Returns where the pinned
/// range ends.
fn pin_backpointer_nodes_with_missing(fs: &Fs, trans: &BtreeTrans<'_>, pins: &BtreeCachePins<'_>,
                                      start: c::bpos) -> Result<c::bpos, BchError> {
    let node_size = i64::from(fs.opts.btree_node_size);
    let mut tmp = BkeyBuf::new();

    pins.unpin();

    let mut mem_may_pin = pins.budget() as i64;
    let mut iter = BtreeIter::new_level(trans, c::btree_id::backpointers, start, 1, BtreeIterFlags::PREFETCH);
    iter.for_each(trans, |t, iter, k| {
        if !backpointer_node_has_missing(fs, k) {
            return Ok(ControlFlow::Continue(()));
        }

        mem_may_pin -= node_size;
        if mem_may_pin <= 0 {
            return Ok(ControlFlow::Break(()));
        }

        tmp.reassemble(k);
        iter.prefetch_child(t, tmp.as_ref())?;
        Ok(ControlFlow::Continue(()))
    })?;

    let mut end = SPOS_MAX;
    let mut pinned = SPOS_MAX;
    let mut mem_may_pin = pins.budget() as i64;
    let mut iter = BtreeIter::new_level(trans, c::btree_id::backpointers, start, 1, BtreeIterFlags::PREFETCH);
    iter.for_each(trans, |t, _, k| {
        if !backpointer_node_has_missing(fs, k) {
            return Ok(ControlFlow::Continue(()));
        }

        mem_may_pin -= node_size;
        if mem_may_pin <= 0 {
            end = pinned;
            return Ok(ControlFlow::Break(()));
        }

        tmp.reassemble(k);
        btree_node_get_and_pin(t, pins, tmp.as_ref(), c::btree_id::backpointers, 0)?;
        pinned = tmp.as_ref().k.p;
        Ok(ControlFlow::Continue(()))
    })?;

    Ok(end)
}

fn check_extents_to_backpointers(fs: &Fs) -> Result<(), BchError> {
    let trans = crate::btree_trans!(fs);
    let pins = fs.btree_cache_pins();
    let mut s = ExtentsToBpState { bp_range: POS_MIN..=SPOS_MAX, last_flushed: WbMaybeFlush::new()? };
    let mut checked_bad = POS_MAX;

    let mut iter = BtreeIter::new(&trans, c::btree_id::alloc, POS_MIN, BtreeIterFlags::PREFETCH);
    iter.for_each(&trans, |t, _, k| {
        passes::cancelled(fs)?;
        check_bucket_backpointer_mismatch(t, k, s.last_flushed.borrow(), &mut checked_bad)?;
        Ok(ControlFlow::<()>::Continue(()))
    })?;

    let (mut nr_buckets, mut nr_mismatches, mut nr_empty) = (0u64, 0u64, 0u64);
    for ca in fs.members() {
        nr_buckets += ca.mi.nbuckets;
        nr_mismatches += ca.bucket_backpointer_mismatch.nr();
        nr_empty += ca.bucket_backpointer_empty.nr();
    }

    #[cfg(not(CONFIG_BCACHEFS_DEBUG))]
    if nr_mismatches == 0 {
        return Ok(());
    }

    crate::bch_info!(fs, "scanning for missing backpointers in {}/{nr_buckets} buckets, \
                          {nr_empty} buckets with no backpointers", nr_mismatches.wrapping_sub(nr_empty));

    let ret = (|| {
        let mut start = POS_MIN;
        loop {
            let end = pin_backpointer_nodes_with_missing(fs, &trans, &pins, start)?;

            if start == POS_MIN && end != SPOS_MAX {
                crate::bch_info!(fs, "bch2_check_extents_to_backpointers(): alloc info does not fit in ram, \
                                      running in multiple passes with {} nodes per pass",
                                 pins.budget() / u64::from(fs.opts.btree_node_size));
            }

            if start != POS_MIN || end != SPOS_MAX {
                crate::bch_verbose!(fs, "check_extents_to_backpointers(): {start}-{end}");
            }

            s.bp_range = start..=end;
            check_extents_to_backpointers_pass(fs, &trans, &mut s)?;

            if end == SPOS_MAX {
                return Ok(());
            }
            start = end.successor();
        }
    })();

    for ca in fs.members() {
        ca.bucket_backpointer_mismatch.free();
        ca.bucket_backpointer_empty.free();
    }
    ret
}

crate::recovery_pass!(bch2_check_extents_to_backpointers => check_extents_to_backpointers);

// ── fsck: check_backpointers_to_extents ──────────────────────────────────

fn check_one_backpointer(t: &TransAttempt<'_, '_>, range: &RangeInclusive<c::bbpos>,
                         bp: BkeySC<'_, c::Backpointer>, last_flushed: WbFlush<'_>)
    -> Result<(), BchError>
{
    if !range.contains(&bp.v().target()) {
        return Ok(());
    }

    let mut iter = BtreeIter::uninit();
    match get_key(t, bp, &mut iter, BtreeIterFlags::empty(), Some(last_flushed), true) {
        Err(e) if e.matches(bch_errcode::BCH_ERR_backpointer_to_overwritten_btree_node) => Ok(()),
        r => r.map(|_| ()),
    }
}

/// Inside the caller's attempt: a restart is the caller's.
fn check_bucket_backpointers_to_extents(trans: &BtreeTrans<'_>, ca: &c::bch_dev, bucket: c::bpos,
                                        last_flushed: WbFlush<'_>) -> Result<(), BchError> {
    let restart_count = trans.restart_count();
    let range = BackpointerPos::bucket_range(ca, bucket);

    BpScanIter::new(c::btree_id::backpointers, *range.start(), None)
        .for_each(trans, *range.end(), last_flushed, |t, bp, last_flushed| {
            check_one_backpointer(t, &(c::bbpos::MIN..=c::bbpos::MAX), bp, last_flushed)
        })?;

    // make sure bad backpointers that were deleted are visible
    trans.write_buffer_flush_sync()?;
    if trans.restart_count() != restart_count {
        return Err(BchError::from(bch_errcode::BCH_ERR_transaction_restart_nested));
    }
    Ok(())
}

fn check_backpointers_to_extents_pass(fs: &Fs, trans: &BtreeTrans<'_>, range: RangeInclusive<c::bbpos>)
    -> Result<(), BchError>
{
    let mut last_flushed = WbMaybeFlush::new()?;
    let btrees = [c::btree_id::backpointers, c::btree_id::stripe_backpointers];
    let progress = Progress::recovery(fs, c"backpointers_to_extents", &btrees, &[]);

    for btree in btrees {
        BpScanIter::new(btree, POS_MIN, Some(&progress))
            .for_each(trans, POS_MAX, last_flushed.borrow(), |t, bp, last_flushed| {
                check_one_backpointer(t, &range, bp, last_flushed)
            })?;
    }
    Ok(())
}

fn check_backpointers_to_extents(fs: &Fs) -> Result<(), BchError> {
    let trans = crate::btree_trans!(fs);
    let pins = fs.btree_cache_pins();
    let leaves = 1 << c::btree_id::extents.0 | 1 << c::btree_id::reflink.0;
    let mut start = c::bbpos::MIN;

    loop {
        let end = pins.pin(&trans, leaves, !0, start..=c::bbpos::MAX)?;

        if start == c::bbpos::MIN && end != c::bbpos::MAX {
            crate::bch_verbose!(fs, "bch2_check_backpointers_to_extents(): extents do not fit in ram, \
                                     running in multiple passes with {} nodes per pass",
                                pins.budget() / u64::from(fs.opts.btree_node_size));
        }

        if start != c::bbpos::MIN || end != c::bbpos::MAX {
            crate::bch_verbose!(fs, "check_backpointers_to_extents(): {start}-{end}");
        }

        check_backpointers_to_extents_pass(fs, &trans, start..=end)?;

        if end == c::bbpos::MAX {
            return Ok(());
        }
        start = end.successor();
    }
}

crate::recovery_pass!(bch2_check_backpointers_to_extents => check_backpointers_to_extents);

// ── Bucket bitmaps ───────────────────────────────────────────────────────

impl c::bucket_bitmap {
    pub fn nr(&self) -> u64 {
        self.bits.nr.get()
    }

    /// Empty if it isn't allocated.
    pub fn words<'a>(&'a self, rcu: &'a ReadGuard) -> &'a [AtomicULong] {
        self.bits.buckets.read(rcu)
    }

    pub fn test(&self, b: u64) -> bool {
        self.bits.buckets.with(|w| AtomicBitmap::from_words(w).test(b as usize))
    }

    /// The first allocates the bitmap.
    pub fn set(&self, ca: &c::bch_dev, b: u64) -> Result<(), BchError> {
        let mut l = self.bits.lock();
        let w = l.published();

        if w.buckets.with(|words| words.is_empty()) {
            w.buckets.rebuild(bits_to_longs(ca.mi.nbuckets as usize), |_| ())
                .or_else(|_| ca.fs().throw(bch_errcode::BCH_ERR_ENOMEM_backpointer_mismatches_bitmap))?;
        }

        if !w.buckets.with(|words| AtomicBitmap::from_words(words).test_and_set(b as usize)) {
            *w.nr.place() += 1;
        }
        Ok(())
    }

    /// Only if it's allocated: a copy, the old words freed after a grace period.
    pub fn resize(&self, ca: &c::bch_dev, nbuckets: u64) -> Result<(), BchError> {
        let mut l = self.bits.lock();
        let w = l.published();

        if w.buckets.with(|old| old.is_empty()) {
            return Ok(());
        }

        w.buckets.rebuild(bits_to_longs(nbuckets as usize), |new| {
            w.buckets.with(|old| {
                for (n, o) in new.iter_mut().zip(old) {
                    *n.get_mut() = o.load(Ordering::Relaxed);
                }
            })
        })
        .or_else(|_| ca.fs().throw(bch_errcode::BCH_ERR_ENOMEM_backpointer_mismatches_bitmap))
    }

    /// The words are freed after a grace period.
    pub fn free(&self) {
        let mut l = self.bits.lock();
        let w = l.published();

        w.buckets.clear();
        w.nr.set(0);
    }
}

#[c_export]
pub fn bch2_bucket_bitmap_test(b: &c::bucket_bitmap, bucket: u64) -> bool {
    b.test(bucket)
}

#[c_export]
pub fn bch2_bucket_bitmap_resize(ca: &c::bch_dev, b: &c::bucket_bitmap, nbuckets: u64) -> c_int {
    ret_to_c(b.resize(ca, nbuckets))
}

#[c_export]
pub fn bch2_bucket_bitmap_free(b: &c::bucket_bitmap) {
    b.free()
}
