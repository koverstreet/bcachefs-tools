// SPDX-License-Identifier: GPL-2.0

//! A stripe being made's blocks: their buckets - allocated new, or carried
//! and moved from an old stripe it reuses - and its slot.

use crate::alloc::lru::{LruId, LruPos};
use crate::btree::bkey::{bkey_gt, bpos_max, pos, BkeyV, BkeyVMut};
use crate::btree::iter::{is_restart, BtreeIter, BtreeIterFlags, CommitFlags, LoopFlow, TransAttempt,
                         UpdateTriggerFlags};
use crate::c;
use crate::data::copygc;
use crate::data::ec::buf::blocks_in;
use crate::data::ec::open::OpenStripe;
use crate::data::ec::repair::StripeBlocks;
use crate::data::ec::stripe::{StripeKey, STRIPE_LRU_POS_EMPTY};
use crate::data::ec::stripe_new::{Internal, StripeBlock};
use crate::data::ec::trigger::ec_stripe_mem_alloc;
use crate::errcode::{bch_errcode, BchError};
use crate::fs::Fs;
use crate::types::Align8U64;
use crate::util::backref::BackRef;
use crate::util::bitmap::Bitmap;
use crate::util::rcu::read_lock;
use crate::util::vstructs::Validated;
use core::ffi::c_uint;
use core::ops::ControlFlow;
use core::sync::atomic::Ordering;

// ── A new stripe's buckets ───────────────────────────────────────────────
//
// A new stripe's blocks are buckets the allocator opens for it, parity
// first, then data, one at a time - each as near as it can be to the
// centroid, where the stripe's other blocks are on average as fractions of
// their devices, so a stripe's blocks are at the same place on each device.
// Their pins are the stripe's, in blocks[], by block. With too few devices
// to fill it, a stripe may shrink to the blocks it got.

/// The centroid of @v's blocks so far, @gotten: stripe_blocks_centroid(). 0
/// for none.
fn blocks_centroid(fs: &Fs, v: &c::Stripe<Validated>, gotten: &Bitmap) -> u64 {
    let rcu = read_lock();
    let (sum, nr) = gotten.iter()
        .filter(|&i| i < usize::from(v.nr_blocks))
        .map(|i| v.block_ptrs()[i])
        .filter(|p| u32::from(p.dev()) != c::BCH_SB_MEMBER_INVALID)
        .filter_map(|p| fs.dev_rcu(p.dev().into(), &rcu).map(|ca| ca.offset_to_frac(p.offset())))
        .fold((0u64, 0u64), |(sum, nr), f| (sum.wrapping_add(f), nr + 1));
    if nr != 0 { sum / nr } else { 0 }
}

/// Blocks @slots of @v, for @want of them, of @data_type, from @stripe's
/// spreading: one at a time, the centroid moving as they're placed.
#[allow(clippy::too_many_arguments)]
fn alloc_blocks(t: &TransAttempt<'_, '_>, req: &mut c::alloc_request, stripe: &mut c::dev_stripe_state,
                v: &mut c::Stripe<Validated>, blocks: &mut [Option<StripeBlock>], stripe_fs: &BackRef<Fs>,
                gotten: &mut c::StripeBlockMask, data_type: c::bch_data_type, slots: core::ops::Range<usize>,
                have: &mut usize)
    -> Result<(), BchError>
{
    let fs = t.fs();
    while *have < slots.len() {
        // the caller's, saved: not ours to put
        req.ptrs.nr = 0;
        req.nr_replicas = (*have + 1) as u8;
        req.nr_effective = *have as c_uint;
        req.data_type = data_type;
        req.target_frac = blocks_centroid(fs, v, gotten);

        let ret = t.bucket_alloc_set(req, stripe);
        for ob in req.ptrs.drain(fs) {
            let j = gotten.next_zero(slots.start, slots.end).expect("a block left to fill");
            req.devs_may_alloc.d.clear(ob.dev() as usize);
            v.block_ptrs_mut()[j] = ob.ptr();
            // SAFETY: @blocks is a stripe being made's, alloc_buckets()' caller's
            blocks[j] = Some(unsafe { StripeBlock::new(ob, stripe_fs) });
            gotten.set(j);
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
                 s: &mut Internal, stripe_fs: &BackRef<Fs>) -> Result<(), BchError> {
    let fs = t.fs();
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
    for i in s.blocks_gotten.iter().filter(|&i| i < nr_blocks) {
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

    let (blocks, gotten) = (&mut s.blocks, &mut s.blocks_gotten);
    alloc_blocks(t, req, &mut stripes.parity_stripe, v, blocks, stripe_fs, gotten,
                 c::bch_data_type::BCH_DATA_parity, nr_data..nr_blocks, &mut have_parity)?;
    alloc_blocks(t, req, &mut stripes.block_stripe, v, blocks, stripe_fs, gotten,
                 c::bch_data_type::BCH_DATA_user, 0..nr_data, &mut have_data)
}

/// Once every block's allocated, the centroid's settled: each new block far
/// from it - by the mean absolute deviation of the others - tried again,
/// nearer, and kept if that halves its distance.
/// Only the new ones: those @had - a reused stripe's, carried over - are
/// anchors, and those a writer's claimed (blocks_allocated) can't move. Best
/// effort, so it never makes things worse: an allocation that fails keeps the
/// original - but a restart, which ends the attempt.
fn reallocate_outliers(t: &TransAttempt<'_, '_>, req: &mut c::alloc_request, s: &mut Internal,
                       stripe_fs: &BackRef<Fs>, had: &Bitmap) -> Result<(), BchError> {
    const MAX: usize = c::BCH_BKEY_PTRS_MAX as usize;
    let fs = t.fs();

    // as device fractions: devices' sizes may differ
    let mut frac = [None::<u64>; MAX];
    {
        let v = s.new_stripe.stripe().v();
        let rcu = read_lock();
        for i in s.blocks_gotten.iter().filter(|&i| i < MAX) {
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
        if had.test(i) || s.blocks_allocated.test(i) {
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
            // SAFETY: into @s's blocks[], a stripe being made's
            let old = s.blocks[i].replace(unsafe { StripeBlock::new(ob, stripe_fs) });
            drop(old.expect("a gotten block's bucket"));
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
    let parity_gotten = (old_nr_data..nr_blocks).filter(|&i| s.blocks_gotten.test(i)).count();
    let new_nr_data = s.blocks_gotten.weight() as usize - parity_gotten;
    let new_nr_blocks = new_nr_data + parity_gotten;

    assert!(s.blocks[new_nr_data..old_nr_data].iter().all(Option::is_none),
            "shrinking a new stripe to {new_nr_data} data blocks, with a bucket for a block past them");

    // down, in order: each to a block that's empty, or was moved from
    for k in 0..parity_gotten {
        s.blocks[new_nr_data + k] = s.blocks[old_nr_data + k].take();
    }
    assert!(s.blocks[new_nr_blocks..].iter().all(Option::is_none),
            "shrinking a new stripe to {new_nr_blocks} blocks, with a bucket for a block past them");

    let v = s.new_stripe.stripe_mut();
    v.block_ptrs_mut().copy_within(old_nr_data..old_nr_data + parity_gotten, new_nr_data);
    for i in old_nr_data..nr_blocks {
        s.blocks_gotten.clear(i);
    }
    for i in new_nr_data..new_nr_blocks {
        s.blocks_gotten.set(i);
    }
    v.block_ptrs_mut()[new_nr_blocks..].fill(c::bch_extent_ptr::from_bits(Align8U64(0)));
    v.nr_blocks = new_nr_blocks as u8;
    v.nr_redundant = parity_gotten as u8;

    let mut devs = c::bch_devs_list::default();
    for (b, p) in s.blocks.iter().zip(v.block_ptrs()) {
        let ob_dev = b.as_ref().map(|b| b.dev());
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
                                       dev_stripe: &c::ec_dev_stripe_state, s: &mut Internal,
                                       stripe_fs: &BackRef<Fs>, may_shrink: bool)
    -> Result<(), BchError>
{
    let fs = t.fs();
    let gotten = |s: &Internal, n: usize| s.blocks_gotten.iter().filter(|&i| i < n).count();
    let (nr_blocks, nr_parity) = {
        let v = s.new_stripe.stripe().v();
        (usize::from(v.nr_blocks), usize::from(v.nr_redundant))
    };
    if gotten(s, nr_blocks) == nr_blocks {
        return Ok(());
    }

    let had = s.blocks_gotten;
    let mut shrunk = false;
    let saved = t.alloc_zeroed::<ReqSaved>()?;
    saved.save(req);

    req.devs_may_alloc = s.devs;
    if req.watermark == c::bch_watermark::BCH_WATERMARK_copygc {
        req.flags |= c::bch_write_flags::BCH_WRITE_alloc_nowait;
    }

    let mut ret = dev_stripe.lock.lock_trans(t).and_then(|mut g| alloc_buckets(t, req, g.protected(), s, stripe_fs));

    // as C's: the errors themselves, not their classes
    let blocked = BchError::from(bch_errcode::BCH_ERR_bucket_alloc_blocked);
    let full = ret.err()
        .filter(|&e| may_shrink && (e == blocked || e == BchError::from(bch_errcode::BCH_ERR_freelist_empty)))
        .filter(|_| will_not_allocate(fs, req, s));
    if let Some(err) = full {
        let moving = s.blocks_moving.weight() as usize;
        // a data block at least, unless a block's being moved - and two, or
        // replication does better
        let need = moving + nr_parity + usize::from(moving == 0) + usize::from(s.old_blocks_nr == 0);

        if s.blocks.iter().flatten().count() >= need {
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
        ret = reallocate_outliers(t, req, s, stripe_fs, &had);
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

// ── Stripe reuse ─────────────────────────────────────────────────────────
//
// A new stripe can be packed into an old one instead of being cut whole from
// new buckets: where the devices are low on space, or a full stripe of new
// buckets can't be had. The stripe lru has the stripes with the most room -
// empty data blocks, and blocks they could be widened by - first. The old
// stripe's live data blocks come forward: carried, in place, if their device
// is good, moved to a new bucket if not. Its empty blocks and its parity are
// left behind, and new buckets fill out the rest.
//
// The old stripe is held open from when it's picked - in the new stripe's
// old_stripe_open, put by its teardown - so the stripe trigger and other
// reusers stay off it. Its blocks are read, and the carried ones folded into
// the new stripe, by the new stripe's task: stripe_new_tasks.rs.

/// Whether @s may be packed into @old, @b its blocks: the same disk label,
/// algorithm, redundancy and size; room for one new block past its live ones;
/// and, the devices of the blocks it would carry taken out of @s's, more
/// devices left than parity blocks. may_reuse_stripe().
fn may_reuse_stripe(s: &Internal, old: &c::Stripe<Validated>, b: StripeBlocks) -> bool {
    let new = s.new_stripe.stripe().v();
    if old.disk_label != new.disk_label ||
       old.bits.algorithm() != new.bits.algorithm() ||
       old.nr_redundant != new.nr_redundant ||
       old.sectors != new.sectors {
        return false;
    }

    let mut devs = s.devs;
    for i in blocks_in(b.live & !b.bad) {
        devs.d.clear(old.block_ptrs()[i].dev().into());
    }

    b.live.count_ones() < new.nr_data() && devs.d.weight() > new.nr_redundant.into()
}

/// Stripe @idx, if @s may be packed into it: opened, into @s's
/// old_stripe_open, its key in @s's old_stripe - and its blocks. None if
/// it's gone - the lru races with the write buffer - can't be reused, or is
/// open already. get_old_stripe().
///
/// An empty one that isn't open is deleted, and one whose can_widen is too
/// high is demoted: each a commit, and transaction_restart_commit, for the
/// caller's walk to pick up from.
fn get_old_stripe(t: &TransAttempt<'_, '_>, s: &mut Internal, idx: u64)
    -> Result<Option<StripeBlocks>, BchError>
{
    let fs = t.fs();
    // intent locked until it's open: the stripe trigger deletes empty
    // stripes that aren't open, and can't open them
    let mut iter = BtreeIter::new(t, c::btree_id::stripes, pos(0, idx),
                                  BtreeIterFlags::INTENT | BtreeIterFlags::NOPRESERVE);
    let (iter, k) = iter.peek_slot_shared(t)?;
    let Some(old) = k.arm::<c::Stripe>() else { return Ok(None) };
    let v = old.v();

    if v.lru_pos() == STRIPE_LRU_POS_EMPTY {
        // the trigger doesn't always delete them: a stripe may still have
        // been open when the last of its data went
        if fs.stripe_is_open(idx) {
            return Ok(None);
        }
        t.delete_at(iter, UpdateTriggerFlags::empty())?;
        return t.commit_lazy(CommitFlags::NO_ENOSPC).map(|()| None);
    }

    // A can_widen gone stale-high - its label's RW members shrank since the
    // last stripes scan - would keep a stripe that can't be widened at the
    // front of the lru, ahead of real candidates. Only demoted here:
    // promotion, devices added or going RW, is the scan's. Against the RW
    // members online or not, as the scan's is, so devices coming and going
    // don't churn every stripe's.
    let want = v.want_can_widen(fs.ec_rw_member_devs_nr(v.disk_label, v.sectors.get()),
                                fs.opts().ec_max_data_blocks.into());
    if v.can_widen() > want {
        t.bkey_make_mut(iter, old, UpdateTriggerFlags::empty())?.v_mut().bits.set_can_widen(want);
        return t.commit_lazy(CommitFlags::NO_ENOSPC).map(|()| None);
    }

    let b = StripeBlocks::get(fs, old)?;
    if !may_reuse_stripe(s, v, b) {
        return Ok(None);
    }
    let Some(h) = OpenStripe::tryget_existing(t, iter)? else { return Ok(None) };
    assert!(s.old_stripe_open.is_none(), "a stripe being made with two old stripes");
    s.old_stripe_open = Some(h.into_claim());
    s.old_stripe.set(old);
    Ok(Some(b))
}

/// Lay @s out from its old stripe: blocks in @carry come forward in place,
/// blocks in @move_ get new buckets, any other block is left behind - and the
/// buckets @s had are put, as they may clash with the old stripe's. A repair
/// may carry every data block; reuse leaves room for a new one.
pub(crate) fn init_new_stripe_from_old(s: &mut Internal, carry: u32, move_: u32, repair: bool) {
    let new = s.new_stripe.stripe().v();
    let (nr_blocks, nr_data) = (usize::from(new.nr_blocks), new.nr_data());
    assert_eq!(s.old_stripe.stripe().v().nr_redundant, new.nr_redundant, "a new stripe from an old one of other redundancy");
    debug_assert!(carry & move_ == 0, "blocks {:#x} both carried and moved", carry & move_);

    for i in s.blocks_gotten.iter().filter(|&i| i < nr_blocks) {
        drop(s.blocks[i].take().expect("a gotten block's bucket"));
    }
    s.blocks_gotten = Default::default();
    s.blocks_allocated = Default::default();

    for i in blocks_in(carry | move_) {
        let j = usize::from(s.old_blocks_nr);
        if carry & 1 << i != 0 {
            s.blocks_gotten.set(j);
        } else {
            s.blocks_moving.set(j);
        }
        s.blocks_allocated.set(j);

        let p = s.old_stripe.stripe().v().block_ptrs()[i];
        s.new_stripe.stripe_mut().block_ptrs_mut()[j] = p;
        s.old_block_map[j] = i as u8;
        s.old_blocks_nr += 1;
        assert!(u32::from(s.old_blocks_nr) + u32::from(!repair) <= nr_data,
                "a new stripe of {nr_data} data blocks, from {} of an old one", s.old_blocks_nr);
    }

    s.have_old_stripe = true;
}

/// Pack @s into the first stripe in the stripe lru it may be packed into:
/// stripe_alloc_blocked if there's none.
fn stripe_reuse(t: &TransAttempt<'_, '_>, s: &mut Internal) -> Result<(), BchError> {
    let fs = t.fs();
    let lru = LruPos::range(LruId::Stripes);
    let mut iter = BtreeIter::new(t, c::btree_id::lru, lru.start().to_bpos(), BtreeIterFlags::empty());
    let found = iter.for_each_max_norestart(t, lru.end().to_bpos(), |_, k| {
        Ok(match get_old_stripe(t, s, k.k.p.offset)? {
            Some(b) => ControlFlow::Break(Some(b)),
            None    => ControlFlow::Continue(()),
        })
    })?;

    let Some(b) = found else { return fs.throw(bch_errcode::BCH_ERR_stripe_alloc_blocked) };
    init_new_stripe_from_old(s, b.live & !b.bad, b.live & b.bad, false);
    Ok(())
}

/// stripe_reuse(), which is an optimization: what can't be reused can be
/// allocated. Only an error that means allocating can't work either - a
/// restart, going read-only - is returned.
fn try_stripe_reuse(t: &TransAttempt<'_, '_>, s: &mut Internal) -> Result<(), BchError> {
    match stripe_reuse(t, s) {
        Err(e) if is_restart(&e) || e.matches(c::EROFS) => Err(e),
        _ => Ok(()),
    }
}

/// Whether @s should be packed into an old stripe before trying for buckets:
/// if it can't have its nr_blocks on devices that aren't low on space, a new
/// stripe could only come out of space that is about to be scarce.
/// ec_should_reuse_stripe(). Where every device is a member of every stripe,
/// one device short of room is enough.
///
/// low_on_space, not wants_space: the latter is exactly copygc's own trigger,
/// so gating on it would mean we only start packing once copygc already has
/// work - work made of the half-empty stripes we would not have cut.
fn should_reuse_stripe(fs: &Fs, s: &Internal) -> bool {
    let mut avail = s.devs;
    avail.d.andnot(&fs.copygc.low_on_space.load());
    avail.d.weight() < s.new_stripe.stripe().v().nr_blocks.into()
}

// ── A new stripe ─────────────────────────────────────────────────────────

/// A slot for @s's new stripe - the first free one from the hint, wrapping
/// round once - opened, into its new_stripe_open, with gc's copy
/// allocated. ENOSPC_stripe_create if none's free below U32_MAX.
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
                assert!(s.new_stripe_open.is_none(), "a stripe being made with two slots");
                s.new_stripe_open = Some(h.into_claim());
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
                          s: &mut Internal, stripe_fs: &BackRef<Fs>) -> Result<(), BchError> {
    if !s.have_old_stripe && should_reuse_stripe(t.fs(), s) {
        try_stripe_reuse(t, s)?;
    }

    let saved = (req.watermark, req.flags);
    req.watermark = c::bch_watermark::BCH_WATERMARK_stripe;
    req.flags |= c::bch_write_flags::BCH_WRITE_alloc_nowait;
    let ret = new_stripe_alloc_buckets(t, req, dev_stripe, s, stripe_fs, false);
    (req.watermark, req.flags) = saved;

    match ret {
        Err(e) if !is_restart(&e) && !e.matches(c::ENOMEM) => {}
        ret => return ret,
    }

    if !s.have_old_stripe {
        try_stripe_reuse(t, s)?;
    }
    new_stripe_alloc_buckets(t, req, dev_stripe, s, stripe_fs, true)
}

/// @s, unless it is already: a slot, buckets from @dev_stripe, the disk
/// reservation for its parity, and its buckets in the table of new stripes'.
pub(crate) fn stripe_alloc_or_reuse(t: &TransAttempt<'_, '_>, req: &mut c::alloc_request,
                                    dev_stripe: &c::ec_dev_stripe_state, s: &mut Internal,
                                    stripe_fs: &BackRef<Fs>) -> Result<(), BchError> {
    if s.new_stripe.idx() == 0 {
        stripe_idx_alloc(t, s)?;
    }
    alloc_or_reuse_buckets(t, req, dev_stripe, s, stripe_fs)?;

    if s.res.as_ref().is_none_or(|r| r.sectors() == 0) {
        // NOFAIL: it doesn't fail
        let _ = s.reserve_parity(stripe_fs, c::bch_reservation_flags::BCH_DISK_RESERVATION_NOFAIL);
    }

    s.buckets_add(t.fs())?;
    s.allocated = true;
    Ok(())
}
