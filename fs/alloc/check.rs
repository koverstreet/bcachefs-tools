// SPDX-License-Identifier: GPL-2.0

//! Alloc btree fsck - check.c's, as it converts: check_alloc_info, which
//! checks the btrees derived from the alloc btree against it - need_discard,
//! freespace and bucket_gens - and check_alloc_to_lru_refs, the lrus; and
//! freespace initialization.
//!
//! The alloc btree is authoritative for its buckets, the others indexes of
//! it: repairs go both ways, entries an alloc key says should be there and
//! aren't, and entries no alloc key accounts for. Keys for buckets that don't
//! exist, in any of them, are deleted - which is why the alloc walk visits
//! keys past a device's last bucket, and for devices that don't exist.
//!
//! An alloc key's check ends at a need_discard or freespace error that isn't
//! fixed - declined, or fsck -n: need_discard_or_freespace_err() returned C's
//! fsck_err_wrap() a 0 for it, which goes to the end of the function.
//!
//! Still C: __bch2_check_freespace_key(), the freespace key check the
//! allocator runs too - its repair, from the allocator, goes through a work
//! item. check_alloc_info calls it for every freespace position.
//!
//! The injection tests for every error here: ktest's alloc-inject.ktest and
//! lru-inject.ktest.

use crate::alloc::background::{alloc_to_v4, BucketGensPos, FreespacePos, NeedDiscardPos};
use crate::alloc::buckets::bucket_to_u64;
use crate::alloc::lru::{lru_check_set, lru_mod, per_dev_fragmentation_lru, LruId, LruPos};
use crate::accounting::{data_type, data_type_from_u8};
use crate::btree::bkey::prelude::*;
use crate::btree::bkey::{bkey_eq, bkey_lt, bkey_min, bpos_lt, pos, BkeySC, POS_MAX, POS_MIN, SPOS_MAX};
use crate::btree::iter::{is_restart, lockrestart_do, polonius_key, BtreeIter, BtreeIterFlags, BtreeTrans,
                         CommitFlags, TransAttempt, UpdateTriggerFlags};
use crate::btree::write_buffer::WbMaybeFlush;
use crate::c;
use crate::errcode::{ret_to_c, ret_to_result_void, BchError};
use crate::fs::Fs;
use crate::init::error::id;
use crate::init::passes;
use crate::init::progress::Progress;
use crate::sb::members::DevRef;
use crate::util::ffi::Opaque;
use crate::util::kernel::{local_clock, READ};
use crate::{bch_err, bch_info, bch_verbose, fsck_err, fsck_err_flags, fsck_err_on};
use crate::c::bch_errcode;
use crate::c::bch_fsck_flags as F;
use core::ffi::c_int;
use cstruct_macros::c_export;

// data/ec/init.c's, until it converts
unsafe extern "C" {
    fn bch2_bucket_nr_stripes(trans: *mut c::btree_trans, bucket: c::bpos) -> c_int;
    fn bch2_check_stripe_refs(trans: *mut c::btree_trans) -> c_int;
}

const KEY_TYPE_SET: u8 = c::bch_bkey_type::KEY_TYPE_set.0 as u8;

// ── Walking the alloc btree, holes and all ───────────────────────────────

/// A key in the alloc btree, or a hole: positions @start to @end with no key.
enum KeyOrHole<'k> {
    Key(BkeySC<'k>),
    Hole { start: c::bpos, end: c::bpos },
}

/// The key at @iter, or the hole there - as far as @end, the end of @iter's
/// leaf node, and U32_MAX positions on, whichever comes first: as
/// BTREE_ITER_slots does for an extents btree, on a btree that isn't one.
/// bch2_get_key_or_hole().
fn get_key_or_hole<'k, 't>(t: &'k TransAttempt<'_, 't>, iter: &'k mut BtreeIter<'t>, mut end: c::bpos)
    -> Result<KeyOrHole<'k>, BchError>
{
    let (iter, k) = iter.peek_slot_shared(t)?;
    if k.k.type_ != c::bch_bkey_type::KEY_TYPE_deleted.0 as u8 {
        return Ok(KeyOrHole::Key(k));
    }

    let start = iter.pos();

    // SAFETY: nothing unlocks while we look
    if let Some(leaf) = unsafe { iter.leaf(t) } {
        let leaf_end = leaf.node().key.k.p;
        if leaf_end != SPOS_MAX {
            end = bkey_min(end, leaf_end.nosnap_successor());
        }
    }
    end = bkey_min(end, pos(start.inode, start.offset.wrapping_add(u64::from(u32::MAX) - 1)));

    // btree node min/max is a closed interval, peek_max() takes a half open
    // one:
    let mut iter2 = iter.copy();
    iter2.peek_max(t, end)?;
    let next = iter2.pos();
    assert!(next.offset < start.offset.wrapping_add(u64::from(u32::MAX)),
            "hole from {start} to {next}: past U32_MAX positions");

    Ok(KeyOrHole::Hole { start, end: next })
}

/// @ca, the device for @dev: bch2_dev_iterate_noerror().
fn dev_iterate(fs: &Fs, ca: &mut Option<DevRef>, dev: u32) {
    if ca.as_ref().is_none_or(|ca| u32::from(ca.dev_idx) != dev) {
        *ca = fs.dev_get(dev);
    }
}

/// @bucket, moved up to @ca's first bucket - or, past its last, to the next
/// device's first, @ca following: false if there's no next device.
/// next_bucket().
fn next_bucket(fs: &Fs, ca: &mut Option<DevRef>, bucket: &mut c::bpos) -> bool {
    if let Some(dev) = ca.as_ref() {
        bucket.offset = bucket.offset.max(dev.mi.first_bucket.into());
        if bucket.offset < dev.mi.nbuckets {
            return true;
        }

        *ca = None;
        bucket.inode += 1;
        bucket.offset = 0;
    }

    *ca = fs.next_dev(bucket.inode as u32);
    if let Some(dev) = ca.as_ref() {
        *bucket = pos(dev.dev_idx.into(), dev.mi.first_bucket.into());
    }
    ca.is_some()
}

/// get_key_or_hole() over the alloc btree, for the buckets there are: a hole
/// outside every device's buckets skipped - to the next device's first
/// bucket, unless a key comes first, a key for a bucket that doesn't exist -
/// and one that runs past its device's last bucket cut there. None at the
/// end; @iter back shared, for updates at the key. @ca: the device of a hole
/// returned. bch2_get_key_or_real_bucket_hole().
fn get_key_or_real_bucket_hole<'k, 't>(
    t:    &'k TransAttempt<'_, 't>,
    iter: &'k mut BtreeIter<'t>,
    ca:   &mut Option<DevRef>,
) -> Result<Option<(&'k BtreeIter<'t>, KeyOrHole<'k>)>, BchError> {
    let fs = t.fs();

    loop {
        let (start, end) = match get_key_or_hole(t, iter, POS_MAX)? {
            KeyOrHole::Key(k) => {
                dev_iterate(fs, ca, k.k.p.inode as u32);
                // XXX polonius: returned here, @iter used below
                let k = unsafe { polonius_key(k) };
                return Ok(Some((&*iter, KeyOrHole::Key(k))));
            }
            KeyOrHole::Hole { start, end } => (start, end),
        };

        dev_iterate(fs, ca, end.inode as u32);

        if !ca.as_ref().is_some_and(|ca| ca.bucket_valid(start.offset)) {
            let mut next = start;
            let next_dev = next_bucket(fs, ca, &mut next);

            if let Some(k) = iter.peek(t)? {
                if !next_dev || bpos_lt(k.k.p, next) {
                    // XXX polonius: returned here, @iter used below
                    let k = unsafe { polonius_key(k) };
                    return Ok(Some((&*iter, KeyOrHole::Key(k))));
                }
            }

            if !next_dev {
                return Ok(None);
            }

            iter.set_pos(next);
            continue;
        }

        let nbuckets = ca.as_ref().expect("checked above").mi.nbuckets;
        let end = if end.offset > nbuckets { pos(end.inode, nbuckets) } else { end };

        return Ok(Some((&*iter, KeyOrHole::Hole { start, end })));
    }
}

// ── Alloc keys ───────────────────────────────────────────────────────────

/// A bucket in alloc key @alloc_k incorrectly @set, or not, in need_discard
/// (@discard) or freespace: reported, and, if @repair, offered to fix.
/// Ok(false) - ignored, or not fixed - ends the check of @alloc_k.
fn need_discard_or_freespace_err(
    t:       &TransAttempt<'_, '_>,
    alloc_k: BkeySC<'_>,
    set:     bool,
    discard: bool,
    repair:  bool,
) -> Result<bool, BchError> {
    let flags = F::FSCK_CAN_IGNORE | if repair { F::FSCK_CAN_FIX } else { F::empty() };
    let (err, btree) = if discard {
        (id::need_discard_key_wrong, c::btree_id::need_discard)
    } else {
        (id::freespace_key_wrong, c::btree_id::freespace)
    };

    match fsck_err_flags!(t, POS_MIN, flags, err, "bucket incorrectly {}set in {} btree\n{}",
                          if set { "" } else { "un" }, btree, alloc_k.to_text(t.fs())) {
        Err(e) if e.matches(bch_errcode::BCH_ERR_fsck_errors_not_fixed) => Ok(false),
        r => r,
    }
}

/// need_discard_or_freespace_err(), for C's fsck_err_wrap(): fsck_fix, or 0
/// for an error not fixed - the end of the caller - or an error.
///
/// # Safety
/// @trans is in an attempt.
#[c_export]
pub unsafe extern "C" fn bch2_need_discard_or_freespace_err<'t>(
    trans:   &'t BtreeTrans<'t>,
    alloc_k: BkeySC<'_>,
    set:     bool,
    discard: bool,
    repair:  bool,
) -> c_int {
    let t = unsafe { trans.attempt_in_progress() };
    match need_discard_or_freespace_err(&t, alloc_k, set, discard, repair) {
        Ok(true)  => -(bch_errcode::BCH_ERR_fsck_fix.0 as c_int),
        Ok(false) => 0,
        Err(e)    => ret_to_c(Err(e)),
    }
}

/// Alloc key @alloc_k, at @alloc_iter, against need_discard, freespace,
/// bucket_gens and the stripes btree: bch2_check_alloc_key().
fn check_alloc_key<'t>(
    t:            &TransAttempt<'_, 't>,
    alloc_k:      BkeySC<'_>,
    alloc_iter:   &BtreeIter<'t>,
    iters:        &mut DerivedIters<'t>,
    last_flushed: &mut WbMaybeFlush,
) -> Result<(), BchError> {
    let fs = t.fs();
    let bucket = alloc_k.k.p;

    let ca = fs.dev_bucket_tryget_noerror(bucket);
    if fsck_err_on!(t, ca.is_none(), id::alloc_key_to_missing_dev_bucket,
                    "alloc key for invalid device:bucket {}:{}", { bucket.inode }, { bucket.offset })? {
        t.delete_at(alloc_iter, UpdateTriggerFlags::empty())?;
    }
    let Some(ca) = ca else { return Ok(()) };

    if ca.mi.freespace_initialized == 0 {
        return Ok(());
    }

    let mut a = t.alloc_to_v4_mut(alloc_k)?;

    if data_type_from_u8(a.v().data_type) == data_type::need_discard {
        iters.discard.set_pos(NeedDiscardPos::new(bucket, a.v()).to_bpos());
        let (discard_iter, k) = iters.discard.peek_slot_shared(t)?;
        let missing = k.k.type_ != KEY_TYPE_SET;

        if missing {
            last_flushed.maybe_flush(t, alloc_k)?;

            if !need_discard_or_freespace_err(t, alloc_k, false, true, true)? {
                return Ok(());
            }
            t.bit_mod_buffered(c::btree_id::need_discard, discard_iter.pos(), true)?;
        }
    }

    iters.freespace.set_pos(FreespacePos::new(bucket, a.v()).to_bpos());
    let (freespace_iter, k) = iters.freespace.peek_slot_shared(t)?;

    let is_free = data_type_from_u8(a.v().data_type) == data_type::free;
    if (k.k.type_ != 0) != is_free {
        if !need_discard_or_freespace_err(t, alloc_k, !is_free, false, true)? {
            return Ok(());
        }
        t.bit_mod_iter(freespace_iter, is_free)?;
    }

    let gens_pos = BucketGensPos::new(bucket);
    iters.bucket_gens.set_pos(gens_pos.key);
    let (bucket_gens_iter, k) = iters.bucket_gens.peek_slot_shared(t)?;

    let bucket_gen = gens_pos.bucket_gen(k);
    if fsck_err_on!(t, a.v().generation != bucket_gen, id::bucket_gens_key_wrong,
                    "incorrect gen in bucket_gens btree (got {} should be {})\n{}",
                    bucket_gen, a.v().generation, alloc_k.to_text(fs))? {
        let mut g = match k.arm::<c::BucketGens>() {
            Some(g) => t.bkey_reassemble(g)?,
            None => {
                let mut g = t.bkey_alloc_typed::<c::BucketGens>()?;
                g.k_mut().p = gens_pos.key;
                g
            }
        };
        g.v_mut().gens[gens_pos.idx] = a.v().generation;
        t.update(bucket_gens_iter, &g, UpdateTriggerFlags::empty())?;
    }

    let nr_stripes = unsafe { bch2_bucket_nr_stripes(t.raw(), bucket) };
    ret_to_result_void(nr_stripes.min(0))?;
    let nr_stripes = nr_stripes as u32;

    if fsck_err_on!(t, a.v().stripe_refcount != nr_stripes, id::alloc_key_stripe_refcount_wrong,
                    "alloc key with incorrect stripe_refcount, should be {}\n{}",
                    nr_stripes, alloc_k.to_text(fs))? {
        a.v_mut().stripe_refcount = nr_stripes;
        t.update(alloc_iter, &a, UpdateTriggerFlags::empty())?;
    }
    Ok(())
}

/// The alloc btree's hole from @start, buckets of @ca, against the freespace
/// btree - @end cut to the freespace key there: bch2_check_alloc_hole_freespace().
fn check_alloc_hole_freespace<'t>(
    t:              &TransAttempt<'_, 't>,
    ca:             &c::bch_dev,
    start:          c::bpos,
    end:            &mut c::bpos,
    freespace_iter: &mut BtreeIter<'t>,
) -> Result<(), BchError> {
    if ca.mi.freespace_initialized == 0 {
        return Ok(());
    }

    freespace_iter.set_pos(start);
    let (freespace_iter, k) = freespace_iter.peek_slot_shared(t)?;
    *end = bkey_min(k.k.p, *end);

    let p = freespace_iter.pos();
    if fsck_err_on!(t, k.k.type_ != KEY_TYPE_SET, id::freespace_hole_missing,
                    "hole in alloc btree missing in freespace btree\ndevice {} buckets {}-{}",
                    { p.inode }, { p.offset }, { end.offset })? {
        let size = (end.offset - p.offset).min(u32::MAX.into());

        let mut u = t.bkey_alloc_typed::<c::Set>()?;
        u.k_mut().p = pos(p.inode, p.offset + size);
        u.k_mut().size = size as u32;
        t.update(freespace_iter, &u, UpdateTriggerFlags::empty())?;
    }
    Ok(())
}

/// The alloc btree's hole from @start against the bucket_gens btree: gens
/// there zero - @end cut to the bucket_gens key there:
/// bch2_check_alloc_hole_bucket_gens().
fn check_alloc_hole_bucket_gens<'t>(
    t:                &TransAttempt<'_, 't>,
    start:            c::bpos,
    end:              &mut c::bpos,
    bucket_gens_iter: &mut BtreeIter<'t>,
) -> Result<(), BchError> {
    let start_pos = BucketGensPos::new(start);
    let end_pos = BucketGensPos::new(*end);

    bucket_gens_iter.set_pos(start_pos.key);
    let (bucket_gens_iter, k) = bucket_gens_iter.peek_slot_shared(t)?;

    let gens_end = if bkey_eq(start_pos.key, end_pos.key) {
        end_pos.idx
    } else {
        c::KEY_TYPE_BUCKET_GENS_NR as usize
    };

    if let Some(g) = k.arm::<c::BucketGens>() {
        let mut gens = g.v().gens;
        let mut need_update = false;

        for i in start_pos.idx..gens_end {
            let b = BucketGensPos { key: k.k.p, idx: i }.bucket();
            if fsck_err_on!(t, gens[i] != 0, id::bucket_gens_hole_wrong,
                            "hole in alloc btree at {}:{} with nonzero gen in bucket_gens btree ({})",
                            { b.inode }, { b.offset }, gens[i])? {
                gens[i] = 0;
                need_update = true;
            }
        }

        if need_update {
            let mut u = t.bkey_reassemble(g)?;
            u.v_mut().gens = gens;
            t.update(bucket_gens_iter, &u, UpdateTriggerFlags::empty())?;
        }
    }

    *end = bkey_min(*end, BucketGensPos { key: k.k.p.nosnap_successor(), idx: 0 }.bucket());
    Ok(())
}

/// The btrees derived from the alloc btree, that its keys and holes are
/// checked against: an iterator on each.
struct DerivedIters<'t> {
    discard:     BtreeIter<'t>,
    freespace:   BtreeIter<'t>,
    bucket_gens: BtreeIter<'t>,
}

/// The alloc key or hole at @iter, checked and committed, and @iter on past
/// it - false at the end: check_btree_alloc_iter().
fn check_btree_alloc_iter<'t>(
    t:            &TransAttempt<'_, 't>,
    ca:           &mut Option<DevRef>,
    iter:         &mut BtreeIter<'t>,
    iters:        &mut DerivedIters<'t>,
    progress:     &Progress<'_>,
    last_flushed: &mut WbMaybeFlush,
) -> Result<bool, BchError> {
    passes::cancelled(t.fs())?;

    let next = {
        let Some((alloc_iter, k)) = get_key_or_real_bucket_hole(t, iter, ca)? else {
            return Ok(false);
        };

        progress.update(t, alloc_iter)?;

        match k {
            KeyOrHole::Key(k) => {
                check_alloc_key(t, k, alloc_iter, iters, last_flushed)?;
                k.k.p.nosnap_successor()
            }
            KeyOrHole::Hole { start, end } => {
                let mut next = end;
                let ca = ca.as_ref().expect("a hole is in a device's buckets");
                check_alloc_hole_freespace(t, ca, start, &mut next, &mut iters.freespace)?;
                check_alloc_hole_bucket_gens(t, start, &mut next, &mut iters.bucket_gens)?;
                next
            }
        }
    };

    t.commit(None, CommitFlags::NO_ENOSPC)?;

    iter.set_pos(next);
    Ok(true)
}

/// Every alloc key and hole, against need_discard, freespace and bucket_gens:
/// check_btree_alloc().
fn check_btree_alloc(fs: &Fs, trans: &BtreeTrans<'_>) -> Result<(), BchError> {
    let progress = Progress::recovery(fs, c"check_btree_alloc", &[c::btree_id::alloc], &[]);

    let mut iter = BtreeIter::new(trans, c::btree_id::alloc, POS_MIN, BtreeIterFlags::PREFETCH);
    let mut iters = DerivedIters {
        discard:     BtreeIter::new(trans, c::btree_id::need_discard, POS_MIN, BtreeIterFlags::PREFETCH),
        freespace:   BtreeIter::new(trans, c::btree_id::freespace, POS_MIN, BtreeIterFlags::PREFETCH),
        bucket_gens: BtreeIter::new(trans, c::btree_id::bucket_gens, POS_MIN, BtreeIterFlags::PREFETCH),
    };

    let mut last_flushed = WbMaybeFlush::new()?;
    let mut ca = None;

    while lockrestart_do(trans, |t| {
        check_btree_alloc_iter(t, &mut ca, &mut iter, &mut iters, &progress, &mut last_flushed)
    })? {}

    Ok(())
}

// ── need_discard, freespace, bucket_gens ─────────────────────────────────

/// Need_discard entry @iter against the bucket's alloc key:
/// bch2_check_discard_key().
fn check_discard_key(t: &TransAttempt<'_, '_>, iter: &BtreeIter<'_>, last_flushed: &mut WbMaybeFlush)
    -> Result<(), BchError>
{
    let fs = t.fs();
    let p = NeedDiscardPos::from_bpos(iter.pos());
    let bucket = p.bucket;

    if fs.dev_bucket_tryget_noerror(bucket).is_none() {
        if fsck_err!(t, id::need_discard_freespace_key_to_invalid_dev_bucket,
                     "entry in {} btree for nonexistant dev:bucket {}:{}",
                     iter.btree(), { bucket.inode }, { bucket.offset })? {
            t.bit_mod_buffered(c::btree_id::need_discard, iter.pos(), false)?;
        }
        return Ok(());
    }

    let mut alloc_iter = BtreeIter::new(t, c::btree_id::alloc, bucket, BtreeIterFlags::empty());
    let alloc_k = alloc_iter.peek_slot(t)?.expect("a slot always has a key");
    let a = alloc_to_v4(alloc_k);

    if data_type_from_u8(a.data_type) != data_type::need_discard ||
       a.journal_seq_empty != p.journal_seq_empty {
        last_flushed.maybe_flush(t, alloc_k)?;

        if fsck_err!(t, id::need_discard_freespace_key_bad, "{}\nincorrectly set at {}:{}:{}:0",
                     alloc_k.to_text(fs), iter.btree(), { iter.pos().inode }, { iter.pos().offset })? {
            t.bit_mod_buffered(c::btree_id::need_discard, iter.pos(), false)?;
        }
    }
    Ok(())
}

/// Freespace position @iter, against the bucket's alloc key - a bad entry
/// deleted, which ends the attempt - through C's __bch2_check_freespace_key(),
/// until the allocator's async repair converts: bch2_check_freespace_key().
fn check_freespace_key(t: &TransAttempt<'_, '_>, iter: &mut BtreeIter<'_>) -> Result<(), BchError> {
    let mut bucket_gen = 0u8;
    let ret = unsafe {
        c::__bch2_check_freespace_key(t.raw(), iter.raw(), &mut bucket_gen, core::ptr::null_mut(),
                                      F::empty(), core::ptr::null_mut())
    };
    match t.result(ret) {
        Err(e) if e.matches(bch_errcode::BCH_ERR_bucket_not_allocatable) => Ok(()),
        r => r,
    }
}

/// Bucket_gens key @k, at @iter, against the device's buckets - gens for
/// buckets that exist were checked against their alloc keys:
/// bch2_check_bucket_gens_key().
fn check_bucket_gens_key(t: &TransAttempt<'_, '_>, iter: &BtreeIter<'_>, k: BkeySC<'_>)
    -> Result<(), BchError>
{
    let fs = t.fs();
    let g = k.arm::<c::BucketGens>().expect("only bucket_gens keys in the bucket_gens btree");
    let start = BucketGensPos { key: k.k.p, idx: 0 }.bucket().offset;
    let end = BucketGensPos { key: k.k.p.nosnap_successor(), idx: 0 }.bucket().offset;

    let Some(ca) = fs.dev_get(k.k.p.inode as u32) else {
        if fsck_err!(t, id::bucket_gens_to_invalid_dev, "bucket_gens key for invalid device:\n{}",
                     k.to_text(fs))? {
            t.delete_at(iter, UpdateTriggerFlags::empty())?;
        }
        return Ok(());
    };

    let first_bucket = u64::from(ca.mi.first_bucket);
    let nbuckets = ca.mi.nbuckets;

    if fsck_err_on!(t, end <= first_bucket || start >= nbuckets, id::bucket_gens_to_invalid_buckets,
                    "bucket_gens key for invalid buckets:\n{}", k.to_text(fs))? {
        return t.delete_at(iter, UpdateTriggerFlags::empty());
    }

    let mut gens = g.v().gens;
    let mut need_update = false;

    for b in (start..first_bucket).chain(nbuckets..end) {
        let i = (b & u64::from(c::KEY_TYPE_BUCKET_GENS_MASK)) as usize;
        if fsck_err_on!(t, gens[i] != 0, id::bucket_gens_nonzero_for_invalid_buckets,
                        "bucket_gens key has nonzero gen for invalid bucket")? {
            gens[i] = 0;
            need_update = true;
        }
    }

    if need_update {
        let mut u = t.bkey_reassemble(g)?;
        u.v_mut().gens = gens;
        t.update(iter, &u, UpdateTriggerFlags::empty())?;
    }
    Ok(())
}

// ── check_alloc_info ─────────────────────────────────────────────────────

fn check_alloc_info(fs: &Fs) -> Result<(), BchError> {
    let trans = crate::btree_trans!(fs);

    check_btree_alloc(fs, &trans)?;

    {
        let mut last_flushed = WbMaybeFlush::new()?;
        let mut iter = BtreeIter::new(&trans, c::btree_id::need_discard, POS_MIN, BtreeIterFlags::PREFETCH);
        iter.for_each_commit(&trans, None, CommitFlags::NO_ENOSPC, |t, iter, _| {
            passes::cancelled(fs)?;
            check_discard_key(t, iter, &mut last_flushed)
        })?;
    }

    // The freespace btree, every position of its keys:
    {
        let mut iter = BtreeIter::new(&trans, c::btree_id::freespace, POS_MIN, BtreeIterFlags::PREFETCH);
        loop {
            passes::cancelled(fs)?;

            let t = trans.begin();
            let k = match iter.peek(&t) {
                Ok(None)    => break,
                Ok(Some(k)) => Ok(*k.k),
                Err(e)      => Err(e),
            };
            let ret = k.and_then(|k| check_freespace_key(&t, &mut iter).map(|()| k));

            match ret {
                Ok(_) => {}
                Err(e) if is_restart(&e) => continue,
                Err(e) => {
                    if let Ok(k) = k {
                        bch_err!(fs, "while checking {k}");
                    }
                    return Err(e);
                }
            }

            iter.set_pos(iter.pos().nosnap_successor());
        }
    }

    let mut iter = BtreeIter::new(&trans, c::btree_id::bucket_gens, POS_MIN, BtreeIterFlags::PREFETCH);
    iter.for_each_commit(&trans, None, CommitFlags::NO_ENOSPC, |t, iter, k| {
        passes::cancelled(fs)?;
        check_bucket_gens_key(t, iter, k)
    })
}

crate::recovery_pass!(bch2_check_alloc_info => check_alloc_info);

// ── check_alloc_to_lru_refs ──────────────────────────────────────────────

/// Alloc key @alloc_k, at @alloc_iter, against the lrus it should be in - its
/// device's fragmentation lru, and for cached data, its read lru:
/// bch2_check_alloc_to_lru_ref().
fn check_alloc_to_lru_ref<'t>(
    t:            &TransAttempt<'_, 't>,
    alloc_iter:   &BtreeIter<'t>,
    alloc_k:      BkeySC<'_>,
    last_flushed: &mut WbMaybeFlush,
) -> Result<(), BchError> {
    let fs = t.fs();
    let dev = alloc_k.k.p.inode as u32;
    let Some(ca) = fs.dev_get(dev) else { return Ok(()) };

    let mut a = alloc_to_v4(alloc_k);
    let target = bucket_to_u64(alloc_k.k.p);

    let lru_idx = a.lru_idx_fragmentation(&ca);
    if lru_idx != 0 {
        let pos = LruPos::new(LruId::Fragmentation { dev }.id(), lru_idx, target);

        // per_dev_fragmentation_lru upgrade: the per-device entry is either
        // missing or was just written by check_lrus' migration and is sitting
        // unflushed in the write buffer, which reads as missing - either way
        // the lookup can't win, and the write buffer flush it triggers is far
        // more expensive than just recreating the entry unconditionally:
        if !per_dev_fragmentation_lru(fs) {
            lru_mod(t, pos, true)?;
        } else {
            lru_check_set(t, pos, alloc_k, last_flushed.borrow())?;
        }
    }

    if data_type_from_u8(a.data_type) == data_type::cached {
        if fsck_err_on!(t, a.io_time[READ] == 0, id::alloc_key_cached_but_read_time_zero,
                        "cached bucket with read_time 0\n{}", alloc_k.to_text(fs))? {
            let mut a_mut = t.alloc_to_v4_mut(alloc_k)?;
            a_mut.v_mut().io_time[READ] = fs.current_io_time(READ);
            t.update(alloc_iter, &a_mut, UpdateTriggerFlags::NORUN)?;
            a = *a_mut.v();
        }

        lru_check_set(t, LruPos::new(LruId::Read { dev }.id(), a.io_time[READ], target), alloc_k,
                      last_flushed.borrow())?;
    }
    Ok(())
}

fn check_alloc_to_lru_refs(fs: &Fs) -> Result<(), BchError> {
    let mut last_flushed = WbMaybeFlush::new()?;
    let progress = Progress::recovery(fs, c"bch2_check_alloc_to_lru_refs", &[c::btree_id::alloc], &[]);

    let trans = crate::btree_trans!(fs);
    let mut iter = BtreeIter::new(&trans, c::btree_id::alloc, POS_MIN, BtreeIterFlags::PREFETCH);
    iter.for_each_commit(&trans, None, CommitFlags::NO_ENOSPC, |t, iter, k| {
        progress.update(t, iter)?;
        last_flushed.inc();
        check_alloc_to_lru_ref(t, iter, k, &mut last_flushed)
    })?;

    ret_to_result_void(unsafe { bch2_check_stripe_refs(trans.raw()) })
}

crate::recovery_pass!(bch2_check_alloc_to_lru_refs => check_alloc_to_lru_refs);

// ── Freespace initialization ─────────────────────────────────────────────

/// The alloc key or hole at @iter, short of @end, into the freespace,
/// need_discard and need_gc_gens btrees, committed, and @iter on past it:
/// dev_freespace_init_iter().
fn dev_freespace_init_iter<'t>(t: &TransAttempt<'_, 't>, ca: &c::bch_dev, iter: &mut BtreeIter<'t>,
                               end: c::bpos) -> Result<(), BchError> {
    match get_key_or_hole(t, iter, end)? {
        // Live keys, one at a time:
        KeyOrHole::Key(k) => {
            let a = alloc_to_v4(k);
            ret_to_result_void(unsafe {
                c::bch2_bucket_do_freespace_index(t.raw(), core::ptr::from_ref(ca).cast_mut(), k.into(),
                                                  &a, true)
            })?;
            t.commit(None, CommitFlags::NO_ENOSPC)?;

            iter.advance();
        }
        KeyOrHole::Hole { start, end } => {
            let mut freespace = t.bkey_alloc_typed::<c::Set>()?;
            freespace.k_mut().p = end;
            freespace.k_mut().size = (end.offset - start.offset) as u32;

            t.insert(c::btree_id::freespace, freespace, UpdateTriggerFlags::empty())?;
            t.commit(None, CommitFlags::NO_ENOSPC)?;

            iter.set_pos(end);
        }
    }
    Ok(())
}

/// Device @ca's buckets @bucket_start to @bucket_end, from the alloc btree
/// into the freespace, need_discard and need_gc_gens btrees, and its member
/// marked freespace_initialized: bch2_dev_freespace_init().
fn dev_freespace_init(fs: &Fs, ca: &c::bch_dev, bucket_start: u64, bucket_end: u64) -> Result<(), BchError> {
    let dev = u32::from(ca.dev_idx);
    let end = pos(dev.into(), bucket_end);

    assert!(bucket_start <= bucket_end, "bucket range {bucket_start}-{bucket_end}");
    assert!(bucket_end <= ca.mi.nbuckets, "bucket {bucket_end} past {} buckets", ca.mi.nbuckets);

    let trans = crate::btree_trans!(fs);
    let mut iter = BtreeIter::new(&trans, c::btree_id::alloc,
                                  pos(dev.into(), bucket_start.max(ca.mi.first_bucket.into())),
                                  BtreeIterFlags::PREFETCH);

    let mut last_updated = local_clock();
    while bkey_lt(iter.pos(), end) {
        if local_clock() > last_updated + 10 * u64::from(c::NSEC_PER_SEC) {
            bch_info!(fs, "{}: bch2_dev_freespace_init: currently at {}/{}",
                      ca.name().to_str().unwrap_or("?"), { iter.pos().offset }, ca.mi.nbuckets);
            last_updated = local_clock();
        }

        lockrestart_do(&trans, |t| dev_freespace_init_iter(t, ca, &mut iter, end))?;
    }

    fs.sb_lock().fs_member_mut(dev).set_freespace_initialized(true);
    Ok(())
}

#[c_export]
pub fn bch2_dev_freespace_init(c: &Opaque<c::bch_fs>, ca: &c::bch_dev, bucket_start: u64, bucket_end: u64)
    -> c_int
{
    ret_to_c(dev_freespace_init(Fs::from_c(c), ca, bucket_start, bucket_end))
}

/// Every device whose freespace isn't initialized - checked on every mount,
/// for a crash in the middle of adding a device: bch2_fs_freespace_init().
fn fs_freespace_init(fs: &Fs) -> Result<(), BchError> {
    if fs.sb.features & (1 << c::bch_sb_feature::BCH_FEATURE_small_image.0) != 0 {
        return Ok(());
    }

    let mut doing_init = false;
    for ca in fs.members() {
        if ca.mi.freespace_initialized != 0 {
            continue;
        }

        if !doing_init {
            bch_info!(fs, "initializing freespace");
            doing_init = true;
        }

        dev_freespace_init(fs, &ca, 0, ca.mi.nbuckets)?;
    }

    if doing_init {
        fs.write_super(&mut fs.sb_lock());
        bch_verbose!(fs, "done initializing freespace");
    }
    Ok(())
}

#[c_export]
pub fn bch2_fs_freespace_init(c: &Opaque<c::bch_fs>) -> c_int {
    ret_to_c(fs_freespace_init(Fs::from_c(c)))
}
