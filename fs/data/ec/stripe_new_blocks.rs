// SPDX-License-Identifier: GPL-2.0

//! A stripe being made's blocks: their buckets - allocated new, or carried
//! and moved from an old stripe it reuses (stripe_new_reuse.rs) - and its
//! slot.

use crate::btree::bkey::{bkey_gt, bpos_max, pos};
use crate::btree::iter::{is_restart, BtreeIter, BtreeIterFlags, LoopFlow, TransAttempt};
use crate::c;
use crate::data::copygc;
use crate::data::ec::open::OpenStripe;
use crate::data::ec::stripe::StripeKey;
use crate::data::ec::stripe_new::{BlockDevsWriter, Internal, InternalGuard, Slot, StripeBlock};
use crate::data::ec::stripe_new_reuse::{should_reuse_stripe, try_stripe_reuse};
use crate::data::ec::trigger::ec_stripe_mem_alloc;
use crate::errcode::{bch_errcode, BchError};
use crate::fs::Fs;
use crate::types::Align8U64;
use crate::util::backref::BackRef;
use crate::util::bitmap::Bitmap;
use crate::util::rcu::read_lock;
use crate::util::vstructs::Validated;
use core::ffi::c_uint;
use core::sync::atomic::Ordering;

// ── A new stripe's buckets ───────────────────────────────────────────────
//
// A new stripe's blocks are buckets the allocator opens for it, parity
// first, then data, one at a time - each as near as it can be to the
// centroid, where the stripe's other blocks are on average as fractions of
// their devices, so a stripe's blocks are at the same place on each device.
// Their pins are the stripe's, in blocks[], by block. With too few devices
// to fill it, a stripe may shrink to the blocks it got.

/// The centroid of @v's blocks so far, those of @blocks that have one:
/// stripe_blocks_centroid(). 0 for none.
fn blocks_centroid(fs: &Fs, v: &c::Stripe<Validated>, blocks: &[Slot]) -> u64 {
    let rcu = read_lock();
    let (sum, nr) = blocks.iter().enumerate()
        .filter(|(i, b)| *i < usize::from(v.nr_blocks) && b.gotten())
        .map(|(i, _)| v.block_ptrs()[i])
        .filter(|p| u32::from(p.dev()) != c::BCH_SB_MEMBER_INVALID)
        .filter_map(|p| fs.dev_rcu(p.dev().into(), &rcu).map(|ca| ca.offset_to_frac(p.offset())))
        .fold((0u64, 0u64), |(sum, nr), f| (sum.wrapping_add(f), nr + 1));
    if nr != 0 { sum / nr } else { 0 }
}

/// Blocks @slots of @v, for @want of them, of @data_type, from @stripe's
/// spreading: one at a time, the centroid moving as they're placed.
#[allow(clippy::too_many_arguments)]
fn alloc_blocks(t: &TransAttempt<'_, '_>, req: &mut c::alloc_request, stripe: &mut c::dev_stripe_state,
                v: &mut c::Stripe<Validated>, blocks: &mut [Slot], block_devs: &BlockDevsWriter,
                data_type: c::bch_data_type, slots: core::ops::Range<usize>, have: &mut usize)
    -> Result<(), BchError>
{
    let fs = t.fs();
    while *have < slots.len() {
        // the caller's, saved: not ours to put
        req.ptrs.nr = 0;
        req.nr_replicas = (*have + 1) as u8;
        req.nr_effective = *have as c_uint;
        req.data_type = data_type;
        req.target_frac = blocks_centroid(fs, v, blocks);

        let ret = t.bucket_alloc_set(req, stripe);
        for ob in req.ptrs.drain(fs) {
            let j = slots.clone().find(|&j| !blocks[j].gotten()).expect("a block left to fill");
            req.devs_may_alloc.d.clear(ob.dev() as usize);
            v.block_ptrs_mut()[j] = ob.ptr();
            block_devs.add(ob.dev());
            blocks[j].fill(StripeBlock::Pinned(ob));
            *have += 1;
        }
        ret?;
    }
    Ok(())
}

/// The buckets @s hasn't got, parity first, @stripes its label's spreading
/// of them, their lock held: __new_stripe_alloc_buckets(). For erasure
/// coding, distinct failure domains are a hard requirement, not a
/// preference: the allocator won't use a device sharing a domain with a
/// block already placed.
fn alloc_buckets(t: &TransAttempt<'_, '_>, req: &mut c::alloc_request, stripes: &mut c::DevStripes,
                 s: &mut InternalGuard<'_>) -> Result<(), BchError> {
    let fs = t.fs();
    let (s, block_devs) = s.parts();
    let v = s.new_stripe.stripe_mut();
    let (nr_blocks, nr_parity) = (usize::from(v.nr_blocks), usize::from(v.nr_redundant));
    let nr_data = v.nr_data() as usize;

    req.bits.set_new_stripe_alloc(true);
    req.bits.set_failure_domains_required(true);
    // rebuilt from the blocks it has: a request can come twice - a full
    // stripe, then stripe reuse - and the first's released buckets would
    // wrongly exclude their devices' domains
    req.devs_chosen = c::bch_devs_mask::default();
    // as the sector allocator, which we bypass, would
    req.devs_may_alloc.d.and(&fs.allocator.rw_devs[c::bch_data_type::BCH_DATA_user.0 as usize].load());

    let (mut have_data, mut have_parity) = (0, 0);
    for i in (0..nr_blocks).filter(|&i| s.blocks[i].gotten()) {
        let dev = v.block_ptrs()[i].dev();
        // a block on a failed or removed device isn't repaired here: that
        // needs its extents updated, by backpointer
        if u32::from(dev) != c::BCH_SB_MEMBER_INVALID {
            req.devs_may_alloc.d.clear(dev.into());
            // the new blocks away from these' domains
            req.devs_chosen.d.set(dev.into());
        }
        if i < nr_data { have_data += 1 } else { have_parity += 1 }
    }
    assert!(have_data <= nr_data && have_parity <= nr_parity,
            "new stripe with {have_data}/{nr_data} data blocks and {have_parity}/{nr_parity} parity");

    let blocks = &mut s.blocks;
    alloc_blocks(t, req, &mut stripes.parity_stripe, v, blocks, block_devs, c::bch_data_type::BCH_DATA_parity,
                 nr_data..nr_blocks, &mut have_parity)?;
    alloc_blocks(t, req, &mut stripes.block_stripe, v, blocks, block_devs, c::bch_data_type::BCH_DATA_user,
                 0..nr_data, &mut have_data)
}

/// Once every block's allocated, the centroid's settled: each new block far
/// from it - by the mean absolute deviation of the others - tried again,
/// nearer, and kept if that halves its distance.
/// Only the new ones: those @had - a reused stripe's, carried over - are
/// anchors, and those a writer's claimed (Slot::allocated()) can't move. Best
/// effort, so it never makes things worse: an allocation that fails keeps the
/// original - but a restart, which ends the attempt.
fn reallocate_outliers(t: &TransAttempt<'_, '_>, req: &mut c::alloc_request, s: &mut InternalGuard<'_>,
                       had: &Bitmap) -> Result<(), BchError> {
    const MAX: usize = c::BCH_BKEY_PTRS_MAX as usize;
    let fs = t.fs();
    let (s, block_devs) = s.parts();

    // as device fractions: devices' sizes may differ
    let mut frac = [None::<u64>; MAX];
    {
        let v = s.new_stripe.stripe().v();
        let rcu = read_lock();
        for i in (0..MAX).filter(|&i| s.blocks[i].gotten()) {
            let Some(p) = v.block_ptrs().get(i) else { continue };
            if u32::from(p.dev()) != c::BCH_SB_MEMBER_INVALID {
                frac[i] = fs.dev_rcu(p.dev().into(), &rcu).map(|ca| ca.offset_to_frac(p.offset()));
            }
        }
    }
    let nr = frac.iter().flatten().count() as u64;
    if nr < 3 {
        return Ok(());
    }
    let mut sum = frac.iter().flatten().fold(0u64, |sum, &f| sum.wrapping_add(f));

    let flags = req.flags;
    req.flags |= c::bch_write_flags::BCH_WRITE_alloc_nowait;
    let mut ret = Ok(());

    for i in 0..MAX {
        let Some(fi) = frac[i] else { continue };
        if had.test(i) || s.blocks[i].allocated() {
            continue;
        }

        // the others' centroid and deviation: one large outlier inflates a
        // deviation of all of them enough to mask itself
        let centroid = sum.wrapping_sub(fi) / (nr - 1);
        let mad = frac.iter().enumerate()
            .filter_map(|(j, f)| f.filter(|_| j != i))
            .fold(0u64, |mad, f| mad.wrapping_add(f.abs_diff(centroid))) / (nr - 1);
        let dist = fi.abs_diff(centroid);
        if dist <= mad.wrapping_mul(2) {
            continue;
        }

        let dev = s.new_stripe.stripe().v().block_ptrs()[i].dev();
        let Some(ca) = fs.dev_get(dev.into()) else { continue };
        // nothing nearer, within a bucket
        if dist <= ca.offset_to_frac(ca.mi.bucket_size.into()) {
            continue;
        }

        req.target_frac = centroid;
        let ob = match t.bucket_alloc(req, &ca) {
            Ok(ob) => ob,
            Err(e) if is_restart(&e) => {
                ret = Err(e);
                break;
            }
            Err(_) => continue,
        };
        let new_ptr = ob.ptr();
        let new_frac = ca.offset_to_frac(new_ptr.offset());

        // nothing's written to a new block yet: swapping it is a put and a
        // new bucket
        if new_frac.abs_diff(centroid) < dist / 2 {
            block_devs.add(ob.dev());
            let old = core::mem::replace(&mut s.blocks[i], Slot::Bucket(StripeBlock::Pinned(ob)));
            assert!(matches!(old, Slot::Bucket(_)), "reallocating a stripe block that isn't a new bucket");
            sum = sum.wrapping_add(new_frac).wrapping_sub(fi);
            frac[i] = Some(new_frac);
            s.new_stripe.stripe_mut().block_ptrs_mut()[i] = new_ptr;
        }
    }

    req.flags = flags;
    req.target_frac = 0;
    ret
}

/// Whether copygc will run on any of @devs - would free buckets there:
/// copygc_can_run_on_devs().
fn copygc_can_run_on_devs(fs: &Fs, devs: &c::bch_devs_mask) -> bool {
    let _mark = fs.mark_lock().read();
    let rcu = read_lock();
    let any = fs.devs_rcu_in(devs.d.iter(), &rcu).any(|ca| copygc::dev_wait_amount(ca).0 <= 0);
    any
}

/// Whether @s won't get the rest of its buckets: copygc is what's asking, or
/// won't run on the devices that had none - which this leaves in
/// req.devs_may_alloc.
fn will_not_allocate(fs: &Fs, req: &mut c::alloc_request, s: &Internal) -> bool {
    req.devs_may_alloc.d.not_and(&s.devs.d);

    req.watermark == c::bch_watermark::BCH_WATERMARK_copygc || !copygc_can_run_on_devs(fs, &req.devs_may_alloc)
}

/// @s cut down to the blocks it got, of @nr_blocks with @nr_parity: its data
/// blocks, and the parity it got moved down after them. Past its new
/// pointers the key is zero: checksums are written at creation.
fn shrink_to_gotten(s: &mut Internal, nr_blocks: usize, nr_parity: usize) {
    let old_nr_data = nr_blocks - nr_parity;
    let parity_gotten = s.blocks[old_nr_data..nr_blocks].iter().filter(|b| b.gotten()).count();
    let new_nr_data = s.blocks.iter().filter(|b| b.gotten()).count() - parity_gotten;
    let new_nr_blocks = new_nr_data + parity_gotten;

    // The data blocks it got are a prefix - the old stripe's first, all of
    // them with their buckets, as they're allocated lowest first - so what's
    // past them is empty
    assert!(s.blocks[new_nr_data..old_nr_data].iter().all(|b| matches!(b, Slot::Empty)),
            "shrinking a new stripe to {new_nr_data} data blocks, with a block past them");

    // down, in order: each to a block that's empty
    for k in 0..parity_gotten {
        s.blocks[new_nr_data + k] = core::mem::take(&mut s.blocks[old_nr_data + k]);
    }
    assert!(s.blocks[new_nr_blocks..].iter().all(|b| matches!(b, Slot::Empty)),
            "shrinking a new stripe to {new_nr_blocks} blocks, with a block past them");

    let v = s.new_stripe.stripe_mut();
    v.block_ptrs_mut().copy_within(old_nr_data..old_nr_data + parity_gotten, new_nr_data);
    v.block_ptrs_mut()[new_nr_blocks..].fill(c::bch_extent_ptr::from_bits(Align8U64(0)));
    v.nr_blocks = new_nr_blocks as u8;
    v.nr_redundant = parity_gotten as u8;

    let mut devs = c::bch_devs_list::default();
    for (b, p) in s.blocks.iter().zip(v.block_ptrs()) {
        let ob_dev = b.bucket().map(|b| b.dev());
        assert!(ob_dev.is_none_or(|d| d == u32::from(p.dev())), "a shrunk stripe's block not on its bucket's device");
        assert!(!devs.has(p.dev()), "a shrunk stripe with two blocks on device {}", p.dev());
        devs.add(p.dev());
    }
}

/// The buckets @s hasn't got, @dev_stripe its label's spreading of them:
/// new_stripe_alloc_buckets(). With @may_shrink, if the devices it needs are
/// full and copygc won't free buckets there, a stripe that got enough
/// shrinks to what it got - else it's stripe_insufficient_devices, for the
/// write to fall back to replication. The request's left as it was.
pub(crate) fn new_stripe_alloc_buckets(t: &TransAttempt<'_, '_>, req: &mut c::alloc_request,
                                       dev_stripe: &c::ec_dev_stripe_state, s: &mut InternalGuard<'_>,
                                       may_shrink: bool)
    -> Result<(), BchError>
{
    let fs = t.fs();
    let gotten = |s: &Internal, n: usize| s.blocks[..n].iter().filter(|b| b.gotten()).count();
    let (nr_blocks, nr_parity) = {
        let v = s.new_stripe.stripe().v();
        (usize::from(v.nr_blocks), usize::from(v.nr_redundant))
    };
    if gotten(s, nr_blocks) == nr_blocks {
        return Ok(());
    }

    let had = s.gotten();
    let mut shrunk = false;
    let saved = t.alloc_zeroed::<ReqSaved>()?;
    saved.save(req);

    req.devs_may_alloc = s.devs;
    if req.watermark == c::bch_watermark::BCH_WATERMARK_copygc {
        req.flags |= c::bch_write_flags::BCH_WRITE_alloc_nowait;
    }

    let mut ret = dev_stripe.lock.lock_trans(t).and_then(|mut g| alloc_buckets(t, req, g.protected(), s));

    // as C's: the errors themselves, not their classes
    let blocked = BchError::from(bch_errcode::BCH_ERR_bucket_alloc_blocked);
    let full = ret.err()
        .filter(|&e| may_shrink && (e == blocked || e == BchError::from(bch_errcode::BCH_ERR_freelist_empty)))
        .filter(|_| will_not_allocate(fs, req, s));
    if let Some(err) = full {
        let moving = s.blocks.iter().filter(|b| b.moving()).count();
        // a data block at least, unless a block's being moved - and two, or
        // replication does better
        let need = moving + nr_parity + usize::from(moving == 0) + usize::from(s.old_block_map().is_empty());

        if s.blocks.iter().filter_map(Slot::bucket).count() >= need {
            shrink_to_gotten(s, nr_blocks, nr_parity);
            ret = Ok(());
            shrunk = true;
        } else {
            if err == blocked {
                fs.alloc_waiters_unpark();
            }
            ret = fs.throw(bch_errcode::BCH_ERR_stripe_insufficient_devices);
        }
    }

    // not after shrinking, which moved blocks under @had: and a stripe we
    // could barely allocate has no spare buckets to do better with
    if ret.is_ok() && !shrunk && gotten(s, nr_blocks) == nr_blocks {
        ret = reallocate_outliers(t, req, s, &had);
    }

    saved.restore(req);
    req.target_frac = 0;
    ret
}

/// What new_stripe_alloc_buckets() changes in the caller's request, to put
/// back: in the transaction's memory, not the stack - the mask is a bit per
/// member, and this is the bottom of a write path.
#[derive(::pin_init::Zeroable)]
struct ReqSaved {
    flags:          c::bch_write_flags,
    data_type:      c::bch_data_type,
    ptrs:           c::open_buckets,
    nr_replicas:    u8,
    nr_effective:   c_uint,
    devs_may_alloc: c::bch_devs_mask,
}

impl ReqSaved {
    fn save(&mut self, req: &c::alloc_request) {
        self.flags          = req.flags;
        self.data_type      = req.data_type;
        self.ptrs           = req.ptrs;
        self.nr_replicas    = req.nr_replicas;
        self.nr_effective   = req.nr_effective;
        self.devs_may_alloc = req.devs_may_alloc;
    }

    fn restore(&self, req: &mut c::alloc_request) {
        req.flags          = self.flags;
        req.data_type      = self.data_type;
        req.ptrs           = self.ptrs;
        req.nr_replicas    = self.nr_replicas;
        req.nr_effective   = self.nr_effective;
        req.devs_may_alloc = self.devs_may_alloc;
    }
}

/// A new stripe's key in @buf: @nr_data and @nr_parity blocks of @sectors,
/// in @disk_label, by @algorithm - ec_stripe_key_init(). Its can_widen is
/// against the label's RW members, so a new stripe - a repair's replacement
/// too - is one widening finds, not stuck at 0 until a stripes scan or fsck
/// visits it. Checksums are per encoded_extent_max, or coarser if that's too
/// many for a key.
pub(crate) fn new_stripe_key(fs: &Fs, key: &mut StripeKey, algorithm: u8, nr_data: u8, nr_parity: u8, sectors: u16,
                             disk_label: u8) {
    let mut v = c::Stripe::default();
    v.sectors = sectors.into();
    v.bits.set_algorithm(algorithm);
    v.nr_blocks = nr_data + nr_parity;
    v.nr_redundant = nr_parity;
    v.csum_granularity_bits = (fs.opts().encoded_extent_max >> 9).ilog2() as u8;
    v.csum_type = c::bch_csum_type::BCH_CSUM_crc32c.0 as u8;
    v.disk_label = disk_label;
    let can_widen = v.want_can_widen(fs.ec_rw_member_devs_nr(disk_label, sectors),
                                     fs.opts().ec_max_data_blocks.into());
    v.bits.set_can_widen(can_widen);

    while v.val_u64s() as usize > c::BKEY_VAL_U64s_MAX {
        assert!(1u32 << v.csum_granularity_bits < u32::from(sectors) && v.csum_granularity_bits != u8::MAX,
                "a new stripe of {sectors} sectors too big for a key, at checksum granularity {}",
                v.csum_granularity_bits);
        v.csum_granularity_bits += 1;
    }
    key.init(&v);
}

// ── A new stripe ─────────────────────────────────────────────────────────

/// A slot for @s's new stripe - the first free one from the hint, wrapping
/// round once - held open, its slot_claim, and its key put there, with gc's
/// copy allocated. ENOSPC_stripe_create if none's free below U32_MAX.
///
/// XXX: we're going to need a bitrange btree of free stripes
pub(crate) fn stripe_idx_alloc(t: &TransAttempt<'_, '_>, s: &mut Internal) -> Result<(), BchError> {
    let fs = t.fs();
    let min_pos = pos(0, 1);
    let mut start = bpos_max(min_pos, pos(0, fs.ec.stripe_hint.load(Ordering::Relaxed)));

    let mut iter = BtreeIter::new(t, c::btree_id::stripes, start, BtreeIterFlags::SLOTS | BtreeIterFlags::INTENT);
    iter.for_each_norestart(t, |iter, k| {
        fs.ec.stripe_hint.store(iter.pos().offset, Ordering::Relaxed);

        if bkey_gt(k.k.p, pos(0, u32::MAX.into())) {
            if start == min_pos {
                return fs.throw(bch_errcode::BCH_ERR_ENOSPC_stripe_create);
            }
            start = min_pos;
            return Ok(LoopFlow::ContinueFrom(min_pos));
        }

        if k.k.is_deleted() {
            if let Some(h) = OpenStripe::tryget(fs, k.k.p.offset)? {
                ec_stripe_mem_alloc(t, iter)?;
                assert!(s.slot_claim.is_none(), "a stripe being made with two slots");
                s.slot_claim = Some(h.into_claim());
                s.new_stripe.k_mut().p = iter.pos();
                return Ok(LoopFlow::Break(()));
            }
        }
        Ok(LoopFlow::Continue)
    })
}

/// @s's buckets: a full stripe of new ones if they can be had without
/// waiting; else packed into an old stripe, and the rest waited for - the
/// stripe shrunk to what it got, if they can't be had. Packed first, where
/// devices are low on space.
fn alloc_or_reuse_buckets(t: &TransAttempt<'_, '_>, req: &mut c::alloc_request, dev_stripe: &c::ec_dev_stripe_state,
                          s: &mut InternalGuard<'_>) -> Result<(), BchError> {
    if s.reuse.is_none() && should_reuse_stripe(t.fs(), s) {
        try_stripe_reuse(t, s)?;
    }

    let saved = (req.watermark, req.flags);
    req.watermark = c::bch_watermark::BCH_WATERMARK_stripe;
    req.flags |= c::bch_write_flags::BCH_WRITE_alloc_nowait;
    let ret = new_stripe_alloc_buckets(t, req, dev_stripe, s, false);
    (req.watermark, req.flags) = saved;

    match ret {
        Err(e) if !is_restart(&e) && !e.matches(c::ENOMEM) => {}
        ret => return ret,
    }

    if s.reuse.is_none() {
        try_stripe_reuse(t, s)?;
    }
    new_stripe_alloc_buckets(t, req, dev_stripe, s, true)
}

/// @s, unless it is already: a slot, buckets from @dev_stripe, the disk
/// reservation for its parity, and its buckets in the table of new stripes'.
pub(crate) fn stripe_alloc_or_reuse(t: &TransAttempt<'_, '_>, req: &mut c::alloc_request,
                                    dev_stripe: &c::ec_dev_stripe_state, s: &mut InternalGuard<'_>,
                                    stripe_fs: &BackRef<Fs>) -> Result<(), BchError> {
    if s.slot_claim.is_none() {
        stripe_idx_alloc(t, s)?;
    }
    alloc_or_reuse_buckets(t, req, dev_stripe, s)?;

    if s.res.as_ref().is_none_or(|r| r.sectors() == 0) {
        // NOFAIL: it doesn't fail
        let _ = s.reserve_parity(stripe_fs, c::bch_reservation_flags::BCH_DISK_RESERVATION_NOFAIL);
    }

    s.buckets_add(t.fs())?;
    s.allocated = true;
    Ok(())
}
