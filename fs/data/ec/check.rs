// SPDX-License-Identifier: GPL-2.0

//! fsck of what refers to stripes, and what stripes refer to -
//! check_stripe_refs: each stripe's entry in the stripe lru, and a
//! bucket_to_stripe entry for each of its blocks' buckets; and back, that
//! each bucket_to_stripe entry's stripe exists, with a block in that bucket.
//!
//! The stripe trigger maintains both (trigger.rs). A bucket's
//! bucket_to_stripe entries are what its alloc key's stripe_refcount counts,
//! so an entry added or dropped here moves the refcount with it.
//!
//! A block whose bucket's generation has moved on - reused, though the
//! stripe's ref should have kept it - is reported, not repaired (GH #960):
//! what to do with such a stripe is still open.

use crate::alloc::buckets::{bucket_to_u64, u64_to_bucket};
use crate::alloc::lru::{lru_check_set, LruId, LruPos};
use crate::btree::bkey::{pos, BkeySC, BkeyV, POS_MIN};
use crate::btree::iter::{BtreeIter, BtreeIterFlags, BtreeTrans, CommitFlags, TransAttempt, UpdateTriggerFlags};
use crate::btree::write_buffer::WbMaybeFlush;
use crate::c;
use crate::errcode::BchError;
use crate::fs::Fs;
use crate::init::error::id;
use crate::util::rcu::read_lock;
use crate::util::vstructs::Validated;
use crate::{fsck_err, fsck_err_flags, fsck_err_on};

const KEY_TYPE_SET: u8 = c::bch_bkey_type::KEY_TYPE_set.0 as u8;

/// Bucket @bucket's reference to stripe @stripe, added or (!@set) dropped:
/// its bucket_to_stripe entry, and its alloc key's stripe_refcount.
/// bucket_stripe_ref_mod().
fn bucket_stripe_ref_mod(t: &TransAttempt<'_, '_>, bucket: c::bpos, stripe: u64, set: bool)
    -> Result<(), BchError>
{
    let mut a = t.start_alloc_update(bucket, UpdateTriggerFlags::empty())?;
    let r = &mut a.v_mut().stripe_refcount;
    *r = if set { r.wrapping_add(1) } else { r.wrapping_sub(1) };

    t.bit_mod(c::btree_id::bucket_to_stripe, pos(bucket_to_u64(bucket), stripe), set)
}

/// Stripe @k's lru entry and its blocks' buckets' refs to it.
/// check_stripe_refs_one().
fn check_stripe_refs_one(t: &TransAttempt<'_, '_>, k: BkeySC<'_>, last_flushed: &mut WbMaybeFlush)
    -> Result<(), BchError>
{
    let Some(s) = k.arm::<c::Stripe>() else { return Ok(()) };
    let fs = t.fs();
    let idx = k.k.p.offset;

    let lru_idx = s.v().lru_pos();
    if lru_idx != 0 {
        lru_check_set(t, LruPos::new(LruId::Stripes.id(), lru_idx, idx), k, last_flushed.borrow())?;
    }

    for (i, ptr) in s.v().block_ptrs().iter().enumerate() {
        let Some(ca) = fs.dev_get(ptr.dev().into()) else { continue };
        let bucket = ca.ptr_bucket_pos(ptr);

        let mut iter = BtreeIter::new(t, c::btree_id::bucket_to_stripe, pos(bucket_to_u64(bucket), idx),
                                      BtreeIterFlags::empty());
        let missing = iter.peek_slot(t)?.expect("a slot always has a key").k.type_ != KEY_TYPE_SET;
        if fsck_err_on!(t, missing, id::stripe_to_missing_bucket_ref,
                        "stripe block {i} missing bucket ref\n{}", k.to_text(fs))? {
            bucket_stripe_ref_mod(t, bucket, idx, true)?;
        }

        let stale = ca.ptr_stale(ptr, &read_lock()).is_some_and(|n| n > 0);
        if stale {
            fsck_err_flags!(t, POS_MIN, c::bch_fsck_flags::FSCK_CAN_IGNORE, id::stripe_read_ptr_stale,
                            "stripe block {i} points to a bucket that has been reused\n{}", k.to_text(fs))?;
        }
    }
    Ok(())
}

/// Whether stripe @s has a block in @bucket. bucket_matches_stripe().
fn bucket_matches_stripe(fs: &Fs, bucket: c::bpos, s: &c::Stripe<Validated>) -> bool {
    s.block_ptrs().iter()
        .any(|ptr| fs.dev_get(ptr.dev().into()).is_some_and(|ca| ca.ptr_bucket_pos(ptr) == bucket))
}

/// The bucket_to_stripe entry at @r: its stripe there, with a block in its
/// bucket - dropped if not. check_bucket_to_stripe_ref().
fn check_bucket_to_stripe_ref(t: &TransAttempt<'_, '_>, r: c::bpos) -> Result<(), BchError> {
    let fs = t.fs();
    let bucket = u64_to_bucket(r.inode);
    let stripe = r.offset;

    let mut iter = BtreeIter::new(t, c::btree_id::stripes, pos(0, stripe), BtreeIterFlags::empty());
    let k = iter.peek_slot(t)?.expect("a slot always has a key");

    let Some(s) = k.arm::<c::Stripe>() else {
        if fsck_err!(t, id::bucket_stripe_ref_to_missing_stripe, "bucket {}:{} points to missing stripe {stripe}",
                     { bucket.inode }, { bucket.offset })? {
            return bucket_stripe_ref_mod(t, bucket, stripe, false);
        }
        return Ok(());
    };

    if fsck_err_on!(t, !bucket_matches_stripe(fs, bucket, s.v()), id::bucket_stripe_ref_to_incorrect_stripe,
                    "bucket {}:{} doesn't match stripe {stripe}", { bucket.inode }, { bucket.offset })? {
        return bucket_stripe_ref_mod(t, bucket, stripe, false);
    }
    Ok(())
}

/// Every stripe's refs, then every bucket_to_stripe entry's stripe:
/// bch2_check_stripe_refs(), in check_alloc_to_lru_refs.
pub fn check_stripe_refs(trans: &BtreeTrans<'_>) -> Result<(), BchError> {
    let mut last_flushed = WbMaybeFlush::new()?;

    BtreeIter::new(trans, c::btree_id::stripes, POS_MIN, BtreeIterFlags::PREFETCH)
        .for_each_commit(trans, None, CommitFlags::NO_ENOSPC, |t, _, k| check_stripe_refs_one(t, k, &mut last_flushed))?;

    BtreeIter::new(trans, c::btree_id::bucket_to_stripe, POS_MIN, BtreeIterFlags::PREFETCH)
        .for_each_commit(trans, None, CommitFlags::NO_ENOSPC, |t, _, k| check_bucket_to_stripe_ref(t, k.k.p))
}
