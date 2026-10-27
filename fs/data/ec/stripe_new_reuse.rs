// SPDX-License-Identifier: GPL-2.0

//! A stripe being made's old stripe, which it reuses - Reuse - and finding
//! one to reuse.
//!
//! A new stripe can be packed into an old one instead of being cut whole from
//! new buckets: where the devices are low on space, or a full stripe of new
//! buckets can't be had. The stripe lru has the stripes with the most room -
//! empty data blocks, and blocks they could be widened by - first. The old
//! stripe's live data blocks come forward: carried, in place, if their device
//! is good, moved to a new bucket if not. Its empty blocks and its parity are
//! left behind, and new buckets fill out the rest.
//!
//! The old stripe is held open from when it's picked - its Reuse's claim,
//! put by the new stripe's teardown - so the stripe trigger and other
//! reusers stay off it. Its blocks are read, and the carried ones folded into
//! the new stripe, by the new stripe's task: stripe_new_tasks.rs.
//!
//! A repair's rebuild reuses the stripe it replaces the same way, with every
//! data block it can keep carried or moved: repair.rs.

use crate::alloc::lru::{LruId, LruPos};
use crate::btree::bkey::{pos, BkeySC, BkeyV, BkeyVMut};
use crate::btree::iter::{is_restart, BtreeIter, BtreeIterFlags, CommitFlags, TransAttempt, UpdateTriggerFlags};
use crate::c;
use crate::data::ec::buf::{blocks_in, StripeBuf};
use crate::data::ec::open::{OpenStripe, StripeClaim};
use crate::data::ec::repair::StripeBlocks;
use crate::data::ec::stripe::{StripeKey, STRIPE_LRU_POS_EMPTY};
use crate::data::ec::stripe_new::{Internal, Slot};
use crate::errcode::{bch_errcode, BchError};
use crate::fs::Fs;
use crate::util::alloc::{kbox_into_inner, kbox_pin_init, KBox};
use crate::util::vstructs::Validated;
use core::ops::ControlFlow;
use core::pin::Pin;
use pin_init::{init, init_zeroed, Init};

/// The old stripe a stripe being made reuses - a stripe it's packed into, or
/// the one a repair rebuilds: held open, its key, and its buffer, which the
/// carried blocks are folded in from. Which of its blocks come forward is in
/// the new stripe's slots - Slot::Carried and Moving.
///
/// Boxed: it's mostly the key, kilobytes, and Some(Reuse) made by value would
/// be a copy through the stack of the allocation path. The box is only there
/// because pin-init can't initialize an Option in place.
pub(crate) struct Reuse {
    pub(crate) stripe: StripeKey,
    /// The old stripe held open: put with the stripe being made
    open:              StripeClaim,
    pub(crate) buf:    Option<KBox<StripeBuf>>,
    pub(crate) read:   OldRead,
}

/// What's been read of the old stripe.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum OldRead {
    /// Nothing, and no buffer for it yet
    NoBuf,
    /// Its buffer's allocated: the task reads it, and folds the carried
    /// blocks in
    ToRead,
    /// Every block's in its buffer, read: a repair's, which carried them
    Read,
}

impl Reuse {
    /// @old, held open by @open: nothing of it read yet - or the claim
    /// back, for the caller to put, if it can't be allocated.
    pub(crate) fn new(old: BkeySC<'_, c::Stripe<Validated>>, open: StripeClaim)
        -> Result<KBox<Self>, StripeClaim>
    {
        // the initializer runs only once there's memory for it: until then
        // the claim's ours to give back
        let mut open = Some(open);
        let claim = &mut open;
        let init = init!(Reuse {
            stripe <- init_zeroed(),
            open:   claim.take().expect("a claim, taken once"),
            buf:    None,
            read:   OldRead::NoBuf,
        }? BchError).chain(|r| {
            r.stripe.set(old);
            Ok(())
        });
        match kbox_pin_init(init) {
            Ok(r) => Ok(Pin::into_inner(r)),
            Err(_) => Err(open.expect("a claim the initializer didn't run to take")),
        }
    }

    pub(crate) fn put(self: KBox<Self>, fs: &Fs) {
        let Reuse { open, .. } = kbox_into_inner(self);
        open.put(fs);
    }
}

// ── Finding one ───────────────────────────────────────────────────────────

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

/// Stripe @idx, if @s may be packed into it: opened, with its key, as @s's
/// Reuse - and its blocks. None if it's gone - the lru races with the write
/// buffer - can't be reused, or is open already. get_old_stripe().
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
    assert!(s.reuse.is_none(), "a stripe being made with two old stripes");
    match Reuse::new(old, h.into_claim()) {
        Ok(r) => s.reuse = Some(r),
        Err(claim) => {
            claim.put(fs);
            return fs.throw(bch_errcode::BCH_ERR_ENOMEM_ec_new_stripe_alloc);
        }
    }
    Ok(Some(b))
}

/// Lay @s out from its old stripe: data blocks in @carry come forward in
/// place, data blocks in @move_ get new buckets, any other block is left
/// behind - and the buckets @s had are put, as they may clash with the old
/// stripe's. A repair may carry every data block; reuse leaves room for a new
/// one.
pub(crate) fn init_new_stripe_from_old(s: &mut Internal, carry: u32, move_: u32, repair: bool) {
    let old_stripe = s.reuse.as_ref().expect("a new stripe laid out from an old one it doesn't have").stripe.stripe();
    let new = s.new_stripe.stripe().v();
    let (nr_blocks, nr_data) = (usize::from(new.nr_blocks), new.nr_data());
    assert_eq!(old_stripe.v().nr_redundant, new.nr_redundant, "a new stripe from an old one of other redundancy");
    debug_assert!(carry & move_ == 0, "blocks {:#x} both carried and moved", carry & move_);

    for b in &mut s.blocks[..nr_blocks] {
        assert!(matches!(b, Slot::Empty | Slot::Bucket(_)), "a new stripe laid out from an old one twice");
        *b = Slot::Empty;
    }

    for (j, i) in blocks_in(carry | move_).enumerate() {
        let b = old_stripe.data_block(i).unwrap_or_else(|| {
            let idx = old_stripe.k.p.offset;
            panic!("a new stripe laid out from block {i} of stripe {idx}, not one of its data blocks")
        });
        assert!(j as u32 + 1 + u32::from(!repair) <= nr_data,
                "a new stripe of {nr_data} data blocks, from {} of an old one", j + 1);

        let old = b.idx();
        s.blocks[j] = if carry & 1 << i != 0 { Slot::Carried { old } } else { Slot::Moving { old, bucket: None } };
        s.new_stripe.stripe_mut().block_ptrs_mut()[j] = b.ptr();
    }
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
pub(crate) fn try_stripe_reuse(t: &TransAttempt<'_, '_>, s: &mut Internal) -> Result<(), BchError> {
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
pub(crate) fn should_reuse_stripe(fs: &Fs, s: &Internal) -> bool {
    let mut avail = s.devs;
    avail.d.andnot(&fs.copygc.low_on_space.load());
    avail.d.weight() < s.new_stripe.stripe().v().nr_blocks.into()
}
