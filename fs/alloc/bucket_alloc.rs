// SPDX-License-Identifier: GPL-2.0

//! One bucket from a device - foreground.c's bch2_bucket_alloc_trans(): the
//! scans that find a free bucket, and with none free, whether to wait.
//!
//! Free buckets are found in the freespace btree: extents of free buckets,
//! at offsets of genbits | bucket - genbits the high bits of the bucket's gc
//! gen, each value a range of the btree of its own (FreespacePos). From the
//! device's cursor - one per btree_bitmap kind - or for erasure coding, the
//! one nearest a target position, so a stripe's blocks land alike on each
//! device. Before freespace is initialized, the alloc btree is scanned
//! instead.
//!
//! Every read is of committed state, BTREE_ITER_committed: an open bucket,
//! and btree nodes made from it, outlive a transaction restart, so
//! allocation mustn't consume this transaction's own pending updates. A
//! transaction freeing a bucket - the discard path's mark_free - can split a
//! node, and so allocate, inside its own commit: taking the bucket it's
//! freeing would take state that may never commit, and with the bucket open,
//! the retried commit could never free it. And no iterator is preserved:
//! nothing here is worth keeping past the call, and callers retrying in one
//! transaction would leave a path per call behind.
//!
//! A candidate is passed over if it's open, nocow locked, a superblock
//! bucket not marked yet, not to be used (migrate's nouse), or emptied by a
//! journal entry not on disk yet; and checked against its alloc key before
//! it's opened.
//!
//! With nothing free, a request waits on freelist_wait and retries, its wake
//! counter snapshot saying whether a wake was for it - or bails, committing
//! the copies it has, when waiting won't help: alloc_should_bail().

use crate::alloc::background::{FREESPACE_GENBITS_NR, FREESPACE_GENBITS_SHIFT, alloc_to_v4};
use crate::alloc::check::{check_freespace_key, FreespaceCheck};
use crate::alloc::foreground::OpenBucket;
use crate::btree::bkey::pos;
use crate::btree::iter::{BtreeIter, BtreeIterFlags, BtreeTrans, TransAttempt};
use crate::c;
use crate::c::bch_errcode;
use crate::data::copygc::{can_make_progress, dev_wait_amount};
use crate::errcode::{err_ptr, BchError};
use crate::fs::Fs;
use crate::opts::data_type_str;
use crate::sb::counters::events;
use crate::util::Printbuf;
use crate::util::kernel::WRITE;
use crate::util::rcu::read_lock;
use core::sync::atomic::Ordering::Relaxed;
use cstruct_macros::c_export;

use c::alloc_btree_bitmap::{BTREE_BITMAP_ANY, BTREE_BITMAP_NO, BTREE_BITMAP_YES};

const BUCKET_MASK: u64 = (1 << FREESPACE_GENBITS_SHIFT) - 1;

impl c::alloc_request {
    /// Whether bucket @b of @ca is in the wrong place for it: a btree node's
    /// outside the device's btree bitmap, or other data's inside it.
    fn btree_bitmap_mismatch(&self, ca: &c::bch_dev, b: u64) -> bool {
        self.btree_bitmap != BTREE_BITMAP_ANY &&
            (self.btree_bitmap == BTREE_BITMAP_YES) !=
            ca.btree_bitmap_marked_sectors(ca.bucket_to_sector(b), ca.mi.get().bucket_size.into(), false)
    }

    /// Whether a caller will retry it elsewhere if it fails here.
    fn will_retry(&self) -> bool {
        let b = self.bits;
        b.will_retry_target_devices() || b.will_retry_all_devices() || b.will_retry_set_devices()
    }
}

impl Fs {
    /// Whether bucket @b of @ca holds a superblock the alloc btree shows as a
    /// free hole, not marked yet: before the trans_mark_dev_sbs pass, and on
    /// a device without freespace initialized - which a resize has while it
    /// marks those the new range brings in.
    fn superblock_bucket_unmarked(&self, ca: &c::bch_dev, b: u64) -> bool {
        let marked = self.recovery.runner.passes_complete.get() &
            1 << c::bch_recovery_pass::BCH_RECOVERY_PASS_trans_mark_dev_sbs as u32 != 0;
        !(marked && ca.mi.get().freespace_initialized != 0) && ca.is_superblock_bucket(b)
    }

    /// Whether @bucket may be taken: not open, not nocow locked.
    fn may_alloc_bucket(&self, req: &mut c::alloc_request, bucket: c::bpos) -> bool {
        if self.bucket_is_open(bucket.inode as u32, bucket.offset) {
            req.counters.skipped_open += 1;
            false
        } else if self.nocow_locks.is_locked(bucket) {
            req.counters.skipped_nocow += 1;
            false
        } else {
            true
        }
    }

    /// Whether a bucket emptied at journal seq @journal_seq_empty may be
    /// taken: the emptying is on disk.
    fn may_alloc_bucket_journal_seq(&self, req: &mut c::alloc_request, journal_seq_empty: u64) -> bool {
        if journal_seq_empty <= self.journal.flushed_seq_ondisk.get() {
            return true;
        }
        if journal_seq_empty > self.journal.flushing_seq.load(Relaxed) as u64 {
            req.counters.need_journal_commit += 1;
        }
        req.counters.skipped_need_journal_commit += 1;
        false
    }

    /// Bucket @b of @ca, at gen @generation, opened - None if it's not to be.
    fn try_alloc_bucket(&self, ca: &c::bch_dev, req: &mut c::alloc_request, b: u64, generation: u8)
        -> Result<Option<OpenBucket>, BchError>
    {
        if self.superblock_bucket_unmarked(ca, b) {
            return Ok(None);
        }
        if ca.bucket_nouse(b) {
            req.counters.skipped_nouse += 1;
            return Ok(None);
        }

        // traced here, not under freelist_lock: pushing onto the trace can
        // allocate
        self.try_alloc_bucket_locked(ca, req, b, generation).inspect_err(|&e| {
            req.trace_add(u8::MAX, Some(e), 0, 0, false);
        })
    }

    /// Whether the rw devices @req may write to can't hold its copies,
    /// however long it waits: N copies of a block on the largest device need
    /// N-1 elsewhere, so the largest mustn't be bigger than the rest - with
    /// one device removed, it can be.
    fn dev_sizes_mismatched(&self, req: &c::alloc_request) -> bool {
        if req.nr_replicas <= 1 {
            return false;
        }

        let rcu = read_lock();
        let (total, max) = self.rw_devs_rcu(&rcu)
            .filter(|ca| u32::from(ca.mi.get().data_allowed) & 1 << req.data_type.0 != 0)
            .fold((0, 0), |(total, max), ca| (total + ca.mi.get().nbuckets, u64::max(max, ca.mi.get().nbuckets)));
        max > total - max
    }

    /// Whether @req, with nothing free on @ca, should commit what it has
    /// instead of waiting - if it has a copy, and waiting won't help: @ca is
    /// leaving (device removal waits on our open buckets), copygc can't free
    /// anything on it, @req is copygc's own - waiting would deadlock it on
    /// itself (not btree nodes, which mustn't commit degraded: a node on a
    /// failing device would take the filesystem read-only) - or the devices
    /// can't hold its copies at all.
    fn alloc_should_bail(&self, ca: &c::bch_dev, req: &c::alloc_request) -> bool {
        let have_replicas = req.nr_effective != 0 || req.devs_have().is_some_and(|d| d.nr != 0);

        have_replicas &&
            (!self.allocator.rw_devs[req.data_type.0 as usize].test(ca.dev_idx.into()) ||
             !req.bits.copygc_can_make_progress() ||
             (req.watermark == c::bch_watermark::BCH_WATERMARK_copygc && req.data_type != c::bch_data_type::BCH_DATA_btree) ||
             self.dev_sizes_mismatched(req))
    }

    #[cold]
    #[inline(never)]
    fn bucket_alloc_to_text(&self, out: &mut Printbuf, ca: &c::bch_dev, req: &c::alloc_request,
                            ret: Result<&OpenBucket, BchError>) {
        out.tabstop_push(32);

        let copygc_dev_wait = {
            let _mark = self.mark_lock().read();
            dev_wait_amount(ca).0
        };
        write!(out, "dev\t{} ({})\n", ca, ca.dev_idx);
        write!(out, "avail\t{}\n", ca.free_buckets_at(&req.usage, req.watermark));
        write!(out, "copygc dev wait\t{copygc_dev_wait}\n");
        write!(out, "watermark\t{}\n", req.watermark.name());
        write!(out, "data type\t{}\n", data_type_str(req.data_type));
        write!(out, "will_retry_target_devices\t{}\n", u8::from(req.bits.will_retry_target_devices()));
        write!(out, "will_retry_all_devices\t{}\n", u8::from(req.bits.will_retry_all_devices()));
        write!(out, "will_retry_set_devices\t{}\n", u8::from(req.bits.will_retry_set_devices()));
        write!(out, "blocking\t{}\n", u8::from(!req.flags.contains(c::bch_write_flags::BCH_WRITE_alloc_nowait)));
        write!(out, "free\t{}\n", req.usage.buckets[c::bch_data_type::BCH_DATA_free.0 as usize]);
        write!(out, "need_discard\t{}\n", req.usage.buckets[c::bch_data_type::BCH_DATA_need_discard.0 as usize]);
        write!(out, "copygc_wait\t{}\n",
               self.copygc.wait.get().wrapping_sub(self.io_clock[WRITE].now.load(Relaxed)));
        write!(out, "seen\t{}\n", req.counters.buckets_seen);
        write!(out, "open\t{}\n", req.counters.skipped_open);
        write!(out, "need journal commit\t{}\n", req.counters.skipped_need_journal_commit);
        write!(out, "nocow\t{}\n", req.counters.skipped_nocow);
        write!(out, "nouse\t{}\n", req.counters.skipped_nouse);
        write!(out, "mi_btree_bitmap\t{}\n", req.counters.skipped_mi_btree_bitmap);

        match ret {
            Ok(ob)  => write!(out, "allocated\t{}\n", ob.id().bucket()),
            Err(e)  => write!(out, "err\t{e}\n"),
        };
    }
}

impl TransAttempt<'_, '_> {
    /// Freespace btree offset @p of @ca - genbits | bucket - checked against
    /// the bucket's alloc key, and opened.
    fn try_alloc_bucket_pos(&self, ca: &c::bch_dev, req: &mut c::alloc_request, p: u64)
        -> Result<Option<OpenBucket>, BchError>
    {
        let fs = self.fs();
        let b = p & BUCKET_MASK;

        if !fs.may_alloc_bucket(req, pos(ca.dev_idx.into(), b)) {
            return Ok(None);
        }

        let iter = BtreeIter::new(self, c::btree_id::BTREE_ID_freespace, pos(ca.dev_idx.into(), p),
                                  BtreeIterFlags::NOPRESERVE);
        let (generation, journal_seq_empty) = match check_freespace_key(self, &iter, FreespaceCheck::Allocator) {
            Err(e) if e == bch_errcode::BCH_ERR_bucket_not_allocatable.into() => return Ok(None),
            r => r?,
        };

        if !fs.may_alloc_bucket_journal_seq(req, journal_seq_empty) {
            return Ok(None);
        }
        fs.try_alloc_bucket(ca, req, b, generation)
    }

    /// Before the freespace btree's initialized: the alloc btree, scanned
    /// from the device's cursor, wrapping once. Uncached, not to fill the key
    /// cache - but the cached key is what serializes allocating a bucket, so
    /// a candidate's is looked up before it's taken.
    #[inline(never)]
    fn bucket_alloc_early(&self, ca: &c::bch_dev, req: &mut c::alloc_request)
        -> Result<Option<OpenBucket>, BchError>
    {
        let first_bucket = u64::from(ca.mi.get().first_bucket);
        let cursor = &ca.alloc_cursor[req.btree_bitmap as usize];
        let mut start = first_bucket.max(cursor.get());

        loop {
            let mut alloc_cursor = start;
            let ret = self.bucket_alloc_early_from(ca, req, &mut alloc_cursor);
            if !matches!(ret, Ok(None)) || start == first_bucket {
                cursor.set(alloc_cursor);
                return ret;
            }
            start = first_bucket;
        }
    }

    /// bucket_alloc_early() from @alloc_cursor, to the end of the device; the
    /// last bucket it looked at left in @alloc_cursor.
    fn bucket_alloc_early_from(&self, ca: &c::bch_dev, req: &mut c::alloc_request, alloc_cursor: &mut u64)
        -> Result<Option<OpenBucket>, BchError>
    {
        let fs = self.fs();
        let shift = ca.mi.get().btree_bitmap_shift;
        let mut iter = BtreeIter::new(self, c::btree_id::BTREE_ID_alloc, pos(ca.dev_idx.into(), *alloc_cursor),
                                      BtreeIterFlags::SLOTS | BtreeIterFlags::COMMITTED);

        loop {
            let Some(k) = iter.peek_slot(self)? else { return Ok(None) };
            let (p, free) = (k.k.p, alloc_to_v4(k).data_type == c::bch_data_type::BCH_DATA_free.0 as u8);
            let b = p.offset;
            *alloc_cursor = b;

            if p.inode != u64::from(ca.dev_idx) || b >= ca.mi.get().nbuckets {
                return Ok(None);
            }

            if req.btree_bitmap_mismatch(ca, b) {
                if req.btree_bitmap == BTREE_BITMAP_YES && ca.bucket_to_sector(b) > 64 << shift {
                    return Ok(None);
                }
                // the next granule, its first bucket next
                let next = ca.sector_to_bucket((ca.bucket_to_sector(b) + 1).next_multiple_of(1 << shift));
                iter.set_pos(pos(ca.dev_idx.into(), next));
                req.counters.buckets_seen += 1;
                req.counters.skipped_mi_btree_bitmap += 1;
                continue;
            }

            if free {
                let mut citer = BtreeIter::new(self, c::btree_id::BTREE_ID_alloc, p,
                    BtreeIterFlags::CACHED | BtreeIterFlags::NOPRESERVE | BtreeIterFlags::COMMITTED);
                let a = alloc_to_v4(citer.peek_slot(self)?.expect("a key or a hole, at a bucket"));

                if a.data_type == c::bch_data_type::BCH_DATA_free.0 as u8 {
                    req.counters.buckets_seen += 1;

                    if fs.may_alloc_bucket(req, p) && fs.may_alloc_bucket_journal_seq(req, a.journal_seq_empty) {
                        if let Some(ob) = fs.try_alloc_bucket(ca, req, b, a.generation)? {
                            return Ok(Some(ob));
                        }
                    }
                }
            }
            iter.advance();
        }
    }

    /// The nearest bucket that can be taken, scanning one genbits class of
    /// @ca's freespace btree forwards or backwards from @start - which
    /// belongs to the forwards scan. Candidates that can't be taken - open,
    /// in the wrong btree bitmap region - move the scan on.
    fn bucket_alloc_scan(&self, ca: &c::bch_dev, req: &mut c::alloc_request, genbits: u64, start: u64,
                         forwards: bool) -> Result<Option<OpenBucket>, BchError>
    {
        let dev = u64::from(ca.dev_idx);
        let end = pos(dev, genbits | BUCKET_MASK);
        let min = genbits | u64::from(ca.mi.get().first_bucket);
        let shift = ca.mi.get().btree_bitmap_shift;

        let mut p = start;
        if !forwards {
            if p == min {
                return Ok(None);
            }
            p -= 1;
        }

        let trim = if forwards { BtreeIterFlags::TRIM } else { BtreeIterFlags::empty() };
        let mut iter = BtreeIter::new(self, c::btree_id::BTREE_ID_freespace, pos(dev, p),
                                      BtreeIterFlags::COMMITTED | BtreeIterFlags::NOPRESERVE | trim);
        loop {
            if !forwards && p < min {
                return Ok(None);
            }

            iter.set_pos(pos(dev, p));
            let k = match forwards {
                true  => iter.peek_max(self, end)?,
                false => iter.peek_prev_min(self, pos(dev, min))?,
            };
            let Some(k) = k else { return Ok(None) };

            // forwards, trimmed: the free extent starts at @p at the earliest
            let cand = if forwards { k.k.start_offset() } else { p.min(k.k.p.offset - 1) };
            let b = cand & BUCKET_MASK;

            req.counters.buckets_seen += 1;

            if req.btree_bitmap_mismatch(ca, b) {
                req.counters.skipped_mi_btree_bitmap += 1;

                // the next granule, or the previous
                if forwards {
                    if req.btree_bitmap == BTREE_BITMAP_YES && ca.bucket_to_sector(b) > 64 << shift {
                        return Ok(None);
                    }
                    p = genbits | ca.sector_to_bucket(ca.bucket_to_sector(b + 1).next_multiple_of(1 << shift));
                } else {
                    // the bucket with the sector before the granule: one
                    // can start before it - bucket sizes needn't be powers
                    // of two - and straddle the two
                    let granule = ca.bucket_to_sector(b) & !((1 << shift) - 1);
                    if granule == 0 {
                        return Ok(None);
                    }
                    p = genbits | ca.sector_to_bucket(granule - 1);
                }
                continue;
            }

            if let Some(ob) = self.try_alloc_bucket_pos(ca, req, cand)? {
                return Ok(Some(ob));
            }

            p = match forwards {
                true                 => cand + 1,
                false if cand == min => return Ok(None),
                false                => cand - 1,
            };
        }
    }

    /// From @ca's cursor: forwards through the genbits classes from there,
    /// then from the start of the device.
    fn bucket_alloc_freelist(&self, ca: &c::bch_dev, req: &mut c::alloc_request)
        -> Result<Option<OpenBucket>, BchError>
    {
        let first_bucket = u64::from(ca.mi.get().first_bucket);
        let cursor = &ca.alloc_cursor[req.btree_bitmap as usize];
        let mut start = first_bucket.max(cursor.get());

        loop {
            for g in start >> FREESPACE_GENBITS_SHIFT..FREESPACE_GENBITS_NR {
                let genbits = g << FREESPACE_GENBITS_SHIFT;
                if let Some(ob) = self.bucket_alloc_scan(ca, req, genbits, start.max(genbits | first_bucket), true)? {
                    cursor.set(genbits | ob.id().bucket());
                    return Ok(Some(ob));
                }
            }

            if start == first_bucket {
                return Ok(None);
            }
            start = first_bucket;
        }
    }

    /// For erasure coding: the bucket nearest @req.target_frac - a fraction
    /// of the device, so a stripe's blocks land alike on devices of any
    /// size. Each genbits class scanned both ways from the target, the
    /// nearest of them kept - nearness over the cursor path's order of
    /// classes, and buckets opened only to be put are cheap. The cursor's
    /// left alone: stripes being made don't drag it about, or each other.
    fn bucket_alloc_target(&self, ca: &c::bch_dev, req: &mut c::alloc_request)
        -> Result<Option<OpenBucket>, BchError>
    {
        let target = ca.sector_to_bucket(ca.frac_to_offset(req.target_frac))
            .max(ca.mi.get().first_bucket.into())
            .min(ca.mi.get().nbuckets - 1);
        let mut best: Option<OpenBucket> = None;

        for g in 0..FREESPACE_GENBITS_NR {
            let genbits = g << FREESPACE_GENBITS_SHIFT;

            for forwards in [false, true] {
                let Some(ob) = self.bucket_alloc_scan(ca, req, genbits, genbits | target, forwards)? else {
                    continue;
                };
                let dist = |ob: &OpenBucket| ob.id().bucket().abs_diff(target);
                if best.as_ref().is_none_or(|best| dist(&ob) < dist(best)) {
                    best = Some(ob);
                }

                if best.as_ref().is_some_and(|best| best.id().bucket() == target) {
                    return Ok(best);
                }
            }
        }
        Ok(best)
    }

    /// From the freespace btree - by @req.target_frac if it has one - or the
    /// alloc btree before freespace is initialized; then with the btree
    /// bitmap ignored, and before check_alloc_info, from the alloc btree.
    fn bucket_alloc_from(&self, ca: &c::bch_dev, req: &mut c::alloc_request, mut freespace: bool)
        -> Result<Option<OpenBucket>, BchError>
    {
        loop {
            let ob = match (freespace, req.target_frac != 0) {
                (true, true)  => self.bucket_alloc_target(ca, req)?,
                (true, false) => self.bucket_alloc_freelist(ca, req)?,
                (false, _)    => self.bucket_alloc_early(ca, req)?,
            };
            if ob.is_some() {
                return Ok(ob);
            }

            if req.btree_bitmap != BTREE_BITMAP_ANY {
                req.btree_bitmap = BTREE_BITMAP_ANY;
            } else if freespace &&
                      self.fs().recovery.runner.pass_done.get() < c::bch_recovery_pass::BCH_RECOVERY_PASS_check_alloc_info {
                freespace = false;
            } else {
                return Ok(None);
            }
        }
    }

    /// A bucket for @req on @ca, with a pin. With none free: freelist_empty if
    /// @req may not block, bucket_alloc_no_progress if it should bail -
    /// otherwise its closure waits on freelist_wait, and it looks once more:
    /// bucket_alloc_blocked if there's still none.
    pub fn bucket_alloc(&self, req: &mut c::alloc_request, ca: &c::bch_dev) -> Result<OpenBucket, BchError> {
        let fs = self.fs();
        let freespace = ca.mi.get().freespace_initialized != 0;
        let mut waiting = false;
        let mut copygc_can_make_progress = false;

        req.btree_bitmap = match req.data_type == c::bch_data_type::BCH_DATA_btree {
            true  => BTREE_BITMAP_YES,
            false => BTREE_BITMAP_NO,
        };
        req.counters = Default::default();

        let (ret, wake_counter_snapshot, avail) = loop {
            let wake_counter_snapshot = ca.alloc_wake_counter.load(Relaxed) as u32;
            req.usage = ca.usage_read_fast();
            let avail = ca.free_buckets_at(&req.usage, req.watermark);

            if req.usage.buckets[c::bch_data_type::BCH_DATA_need_discard.0 as usize] > avail.min(ca.mi.get().nbuckets >> 7) {
                fs.do_discards_async();
            }
            if req.usage.buckets[c::bch_data_type::BCH_DATA_need_gc_gens.0 as usize] > avail {
                fs.gc_gens_async();
            }
            if ca.should_invalidate_buckets(&req.usage) != 0 {
                ca.do_invalidates();
            }

            // before check_allocations the counts may be wrong: above the
            // normal watermark, look anyway
            if avail == 0 && !(req.watermark as u32 > c::bch_watermark::BCH_WATERMARK_normal as u32 &&
                               fs.recovery.runner.pass_done.get() <
                               c::bch_recovery_pass::BCH_RECOVERY_PASS_check_allocations) {
                copygc_can_make_progress = {
                    let _mark = fs.mark_lock().read();
                    can_make_progress(ca)
                };
                if copygc_can_make_progress {
                    req.bits.set_copygc_can_make_progress(true);
                    fs.copygc_wakeup();
                }

                fs.times[c::bch_time_stats::BCH_TIME_blocked_allocate as usize].track_event_change(true);

                let may_block = !req.cl.is_null() &&
                    !req.flags.contains(c::bch_write_flags::BCH_WRITE_alloc_nowait) &&
                    !req.will_retry();
                let err = if !may_block {
                    bch_errcode::BCH_ERR_freelist_empty
                } else if fs.alloc_should_bail(ca, req) {
                    bch_errcode::BCH_ERR_bucket_alloc_no_progress
                } else if !waiting {
                    req.closure_wait(&fs.allocator.freelist_wait);
                    waiting = true;
                    continue;
                } else {
                    bch_errcode::BCH_ERR_bucket_alloc_blocked
                };
                break (Err(fs.err(err)), wake_counter_snapshot, avail);
            }

            if avail != 0 && waiting {
                ca.alloc_wake();
            }
            let ob = self.bucket_alloc_from(ca, req, freespace)
                .and_then(|ob| ob.ok_or_else(|| fs.err(bch_errcode::BCH_ERR_no_buckets_found)));
            break (ob, wake_counter_snapshot, avail);
        };

        match &ret {
            Ok(ob) => {
                ob.data_type.set(req.data_type.0 as u8);

                // an open bucket's gone: the journal throttles what needs
                // what the pool can no longer serve, so reclaim - which needs
                // open buckets to free them - isn't starved
                fs.journal.set_open_buckets_watermark(fs.open_buckets_starved_watermark());

                fs.event_inc_trace(events::bucket_alloc, |out| fs.bucket_alloc_to_text(out, ca, req, Ok(ob)));
            }
            Err(e) if e.matches(bch_errcode::BCH_ERR_open_buckets_empty) ||
                      e.matches(bch_errcode::BCH_ERR_open_bucket_alloc_blocked) => {
                fs.event_inc_trace(events::open_bucket_alloc_fail, |out| fs.fs_open_buckets_to_text(out));
            }
            Err(e) if !e.matches(bch_errcode::BCH_ERR_transaction_restart) && !req.will_retry() => {
                fs.event_inc_trace(events::bucket_alloc_fail, |out| fs.bucket_alloc_to_text(out, ca, req, Err(*e)));
            }
            Err(_) => {}
        }

        req.trace_add(ca.dev_idx, ret.as_ref().err().copied(), wake_counter_snapshot, avail, copygc_can_make_progress);
        ret
    }
}

/// For C: @req.ca the device.
///
/// # Safety
/// @trans is in an attempt; @req.ca is a device the caller holds a ref on.
#[c_export]
pub unsafe fn bch2_bucket_alloc_trans(trans: &BtreeTrans<'_>, req: &mut c::alloc_request) -> *mut c::open_bucket {
    // SAFETY: the caller's
    let (t, ca) = unsafe { (trans.attempt_in_progress(), req.ca.as_ref().expect("a device to allocate on")) };
    match t.bucket_alloc(req, ca) {
        Ok(ob) => {
            let idx = ob.into_idx();
            core::ptr::from_ref(&t.fs().allocator.open_buckets[idx.get()]).cast_mut()
        }
        Err(e) => err_ptr(e),
    }
}
