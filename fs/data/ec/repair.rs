// SPDX-License-Identifier: GPL-2.0

//! Stripe repair: rebuilding a degraded stripe, narrowing it, or
//! rebuilding around what's left of it - and recording, or taking out of
//! the stripe, the extents of blocks that are lost.

use crate::alloc::capacity::DiskReservation;
use crate::alloc::disk_groups::Target;
use crate::btree::bkey::{BkeySC, BkeyV};
use crate::btree::bkey::prelude::*;
use crate::btree::bkey_buf::BkeyBuf;
use crate::btree::iter::{commit_do, lockrestart_do, BtreeIter, BtreeIterFlags, BtreeTrans, CommitFlags, TransAttempt,
                         UpdateTriggerFlags};
use crate::c;
use crate::data::copygc;
use crate::data::ec::buf::{blocks_good, blocks_in, validate_msg, MemWait, StripeBuf, PRE};
use crate::data::ec::delete::stripe_delete;
use crate::data::ec::head::dev_stripe_state_get;
use crate::data::ec::io::read_sync;
use crate::data::ec::open::OpenStripe;
use crate::data::ec::stripe_new::{discard, new_stripes_take, StripeNew};
use crate::data::ec::stripe_new_blocks::{new_stripe_alloc_buckets, stripe_idx_alloc};
use crate::data::ec::stripe_new_reuse::{init_new_stripe_from_old, OldRead, Reuse};
use crate::data::ec::stripe_new_tasks::old_stripe_carry;
use crate::data::ec::update::{block_for_each_extent, Extent};
use crate::data::moving::Moving;
use crate::data::ptrs::{extent_nonce, BkeyVPtrs, BkeyVPtrsMut, Ptrs, PtrsMut};
use crate::data::reconcile::needs_reconcile::update_reconcile_opts;
use crate::errcode::{bch_errcode, ret_to_c, BchError};
use crate::fs::Fs;
use crate::init::damage;
use crate::inode_opts::get_io_opts;
use crate::sb::counters::events;
use crate::util::alloc::KBox;
use crate::util::closure::StackClosure;
use crate::util::rcu::read_lock;
use crate::util::vstructs::Validated;
use crate::{bch_err, c_function_name, log_msg_ratelimited, prt};
use core::ffi::{c_int, c_uint};
use core::mem::size_of;
use core::ptr::NonNull;
use cstruct_macros::c_export;
use pin_init::stack_pin_init;

// ── Recording the extents of blocks lost ──

/// Whether @k's data is good in block @block of @b, stripe @s as read: the
/// block failed the stripe checksum, but an extent elsewhere in it is intact
/// if its own checksum matches what was read. Only for a block that was read
/// - one on a device offline is a buffer of nothing - and no checksum, no
/// telling.
fn extent_good_in_buf(fs: &Fs, k: BkeySC<'_>, s: BkeySC<'_, c::Stripe<Validated>>, b: &StripeBuf, block: u32)
    -> bool
{
    let sp = &s.v().block_ptrs()[block as usize];
    let block_sectors = u64::from(s.v().sectors.get());

    if b.err[PRE][block as usize] != Some(bch_errcode::BCH_ERR_stripe_read_csum_err.into()) ||
       b.data[block as usize].is_none() {
        return false;
    }

    let Some(kp) = k.arm::<Ptrs>() else { return false };
    let Some(d) = kp.ptrs_decoded(fs).find(|d| {
        d.p.ptr.dev() == sp.dev() && d.p.ptr.offset() >= sp.offset() && d.p.ptr.offset() < sp.offset() + block_sectors
    }) else { return false };

    let p = d.p;
    let offset = p.ptr.offset() - sp.offset();
    if p.crc.csum_type == 0 ||
       offset < b.offset.into() ||
       offset + u64::from(p.crc.compressed_size) > u64::from(b.offset + b.size) {
        return false;
    }

    // within the block's buffer, as above
    let start = ((offset - u64::from(b.offset)) << 9) as usize;
    let extent = &b.block(block as usize)[start..start + ((p.crc.compressed_size as usize) << 9)];
    fs.checksum(c::bch_csum_type(p.crc.csum_type.into()), extent_nonce(k.k.bversion, &p.crc), extent) == p.crc.csum
}

/// Damage recorded against every extent in @blocks of stripe @s - found by
/// @s's pointers, so a block on a device being removed already points at
/// BCH_SB_MEMBER_INVALID. Commits per extent: callers record before
/// destroying the blocks, not atomically with it.
///
/// @count: counting the sb error too, as well as recording it. The reuse path
/// doesn't - the read counted the block that failed to reconstruct, and a
/// damage record names an error the counters have heard of, not counting it
/// again per extent. @buf: the stripe as read, if the blocks were read, not
/// lost to a device - an extent that checks out against it isn't lost.
pub fn record_lost_blocks(trans: &BtreeTrans<'_>, s: BkeySC<'_, c::Stripe<Validated>>, blocks: u32,
                          err: c::bch_sb_error_id, count: bool, buf: Option<&StripeBuf>) {
    let fs = trans.fs();

    for block in (0..32).filter(|i| blocks & 1 << i != 0) {
        let ret = block_for_each_extent(trans, s, block, None, CommitFlags::NO_ENOSPC.into(), |t, bp, ext| {
            // reflink extents name no inode to record against
            let Some((_, k)) = ext else { return Ok(()) };
            if bp.v().target().btree != c::btree_id::extents {
                return Ok(());
            }
            if buf.is_some_and(|buf| extent_good_in_buf(fs, k, s, buf, block)) {
                return Ok(());
            }
            if count {
                damage::record_data_loss(t, c::btree_id::extents, k.k.p, err)
            } else {
                damage::record_key(t, c::btree_id::extents, k.k.p, err)
            }
        });

        // a block we can't attribute doesn't stop the others
        if let Err(e) = ret {
            bch_err!(fs, "error recording damage for stripe {} block {block}: {e}", { s.k.p.offset });
        }
    }
}

// ── Taking a block's extents out of its stripe ──

/// Whether @k can be read now: a copy on a device that's online, or erasure
/// coded.
fn readable_now(fs: &Fs, k: BkeySC<'_>) -> bool {
    let rcu = read_lock();
    k.arm::<Ptrs>().is_some_and(|kp| kp.ptrs_decoded(fs).any(|d| {
        d.p.ptr.cached() == 0 &&
            (d.p.has_ec || fs.dev_rcu(d.p.ptr.dev().into(), &rcu).is_some_and(|ca| ca.is_online()))
    }))
}

/// Take an extent out of block @block of stripe @idx, which is being left
/// behind: a pointer to a member device keeps its data and loses the
/// stripe; a pointer to a removed device goes too, and an extent with no copy
/// left becomes an error key. Reconcile re-replicates the rest, against their
/// own checksums. Damage is recorded uncounted: validate counted the stripe's
/// failure once.
fn drop_block_extent(t: &TransAttempt<'_, '_>, idx: u64, block: u32, bp: BkeySC<'_, c::Backpointer>,
                     ext: Extent<'_, '_>) -> Result<(), BchError> {
    let fs = t.fs();

    // gone, or a btree node: nothing of ours
    let Some((iter, k)) = ext else { return Ok(()) };
    let Some(kp) = k.arm::<Ptrs>() else { return Ok(()) };

    let ours = |p: &c::extent_ptr_decoded| p.has_ec && p.ec.idx() == idx && u32::from(p.ec.block()) == block;
    let Some(dev) = kp.ptrs_decoded(fs).find(|d| ours(&d.p)).map(|d| u32::from(d.p.ptr.dev())) else {
        return Ok(());
    };

    let mut n = t.bkey_reassemble_room(k, c::BKEY_EXTENT_U64s_MAX * size_of::<u64>())?;
    {
        let mut kp = n.reborrow().arm::<PtrsMut>().expect("a key with pointers, as it had ours");
        if dev == c::BCH_SB_MEMBER_INVALID {
            kp.drop_ptrs_noerror(fs, |d| ours(&d.p));
        } else {
            kp.drop_ec(fs, dev);
        }
    }

    if n.as_sc().can_read(fs) {
        let opts = get_io_opts(t, None, k)?;
        n.set_needs_reconcile(t, None, &opts, c::set_needs_reconcile_ctx::SET_NEEDS_RECONCILE_opt_change, 0)?;
    } else {
        n.set_error(fs, c::bch_key_type_errors::KEY_TYPE_ERROR_device_removed);
    }

    // reflink extents name no inode to record against
    if !readable_now(fs, n.as_sc()) && bp.v().target().btree == c::btree_id::extents {
        damage::record_key(t, c::btree_id::extents, k.k.p, c::bch_sb_error_id::BCH_FSCK_ERR_stripe_reconstruct_failed)?;
    }

    t.update(iter, &n, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)
}

/// Every extent of block @block of stripe @s taken out of it.
fn drop_block_extents(trans: &BtreeTrans<'_>, s: BkeySC<'_, c::Stripe<Validated>>, block: u32) -> Result<(), BchError> {
    // set_needs_reconcile() may charge for padding pointers
    let res = DiskReservation::new(trans.fs());
    let idx = s.k.p.offset;

    block_for_each_extent(trans, s, block, Some(&res), CommitFlags::NO_ENOSPC.into(),
                          |t, bp, ext| drop_block_extent(t, idx, block, bp, ext))
}

// ── Stripe repair ────────────────────────────────────────────────────────
//
// A degraded stripe - a block on a device that's lost its durability:
// missing, evacuating, or set to durability 0 - is rebuilt at its width if
// there are devices for it. If there aren't, it's narrowed: enough of its live
// data blocks evacuated that what's left fits, and reconcile repairs it again
// once the moves are done. One that can't be reconstructed is rebuilt around
// the blocks that read clean, after taking every other block's extents out of
// it. Scrub repairs a block that failed the stripe's checksums the same way,
// as though it were degraded.
//
// Deciding which comes first; reading the stripe and rebuilding it - the IO,
// and the allocator - after: repair_read(), and repair_rebuild(), which takes
// over the stripe we have open and our read of it, and hands the new stripe
// to its task for its writes.
//
// Repair is a step of the caller's walk, at an intent locked iterator: that
// holds the stripe against its trigger - which deletes empty stripes that
// aren't open, and can't open one - until we have it open. Then it drops the
// locks to read and write, and runs attempts of its own.

/// How a stripe repair that didn't fail went.
#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StripeRepair {
    /// Repaired - rebuilt, handed to stripe creation for its writes, or
    /// deleted, emptied - or nothing to repair, or someone else has the
    /// stripe open.
    Done,
    /// Not finished: repair it again once the IO issued so far is done.
    /// Narrowing's evacuations are moves, which complete later; or an
    /// unrebuildable stripe still had extents in the blocks it was dropping,
    /// which another try may take out.
    Retry,
}

/// What reuse and repair need to know of a stripe's blocks, as masks by block
/// index: which data blocks hold live data, and which blocks - data or parity -
/// are on a device that's lost its durability.
#[derive(Clone, Copy)]
pub(crate) struct StripeBlocks {
    pub(crate) live: u32,
    pub(crate) bad:  u32,
}

impl StripeBlocks {
    pub(crate) fn get(fs: &Fs, s: BkeySC<'_, c::Stripe<Validated>>) -> Result<Self, BchError> {
        let v = s.v();
        let Some(counts) = v.blockcounts() else {
            return fs.throw(bch_errcode::BCH_ERR_stripe_unknown_csum_type);
        };
        let rcu = read_lock();
        let mut b = StripeBlocks { live: 0, bad: 0 };

        for ((i, ptr), n) in (0..).zip(v.block_ptrs()).zip(counts) {
            if i < v.nr_data() && n.get() != 0 {
                b.live |= 1 << i;
            }
            // bch2_stripe_block_dev_bad()
            if fs.dev_durability(ptr.dev().into(), &rcu) == 0 {
                b.bad |= 1 << i;
            }
        }
        Ok(b)
    }
}

/// The devices out of @devs - disk_label_ec_devs()'s - that a repair of @s
/// can use.
///
/// A repair carries the stripe's good data blocks forward where they are -
/// init_new_stripe_from_old() - and only the blocks on bad devices, and all
/// the parity, get new buckets. So a device holding a carried block counts
/// whatever its free space; any other counts only if it can take a new block:
/// free buckets at the repair's watermark now, or copygc will make some - the
/// data update path's test, and the allocator's bail-out.
///
/// Counting the full ones too made a repair on a full filesystem look like it
/// had devices enough, skip narrowing, and fail in the allocator with
/// bucket_alloc_no_progress - every pass, forever.
fn stripe_repair_usable_devs(fs: &Fs, s: BkeySC<'_, c::Stripe<Validated>>, b: StripeBlocks,
                             devs: &mut c::bch_devs_mask) {
    let mut carried = c::bch_devs_mask::default();
    for i in blocks_in(b.live & !b.bad) {
        carried.d.set(s.v().block_ptrs()[i].dev().into());
    }

    let _mark = fs.mark_lock().read();
    let rcu = read_lock();

    let all = *devs;
    for i in all.d.iter().filter(|&i| !carried.d.test(i)) {
        let usable = fs.dev_rcu(i as u32, &rcu).is_some_and(|ca| {
            ca.free_buckets_at(&ca.usage_read_fast(), c::bch_watermark::BCH_WATERMARK_normal) != 0 ||
                copygc::can_make_progress(ca)
        });
        if !usable {
            devs.d.clear(i);
        }
    }
}

/// How many of @s's live data blocks have to be evacuated before what's left
/// fits on the devices a repair can use - 0 if it can be rebuilt at its width
/// - and those devices.
fn stripe_repair_need_evacuate(fs: &Fs, s: BkeySC<'_, c::Stripe<Validated>>, b: StripeBlocks)
    -> (u32, c::bch_devs_mask)
{
    let v = s.v();
    let (mut devs, _) = fs.disk_label_ec_devs(v.disk_label.into(), v.sectors.get().into());
    stripe_repair_usable_devs(fs, s, b, &mut devs);

    // the allocator puts at most one block of a stripe in a failure domain,
    // so devices sharing one can't each take a block
    let nr_usable = devs.d.weight().min(fs.target_nr_domains(&devs));

    ((b.live.count_ones() + u32::from(v.nr_redundant)).saturating_sub(nr_usable), devs)
}

/// Nothing to repair - the device that was evacuating went back to rw, say:
/// clear needs_reconcile, or the stripe stays on the hipri queue and reconcile
/// revisits it forever.
fn stripe_repair_nothing_to_do(t: &TransAttempt<'_, '_>, iter: &BtreeIter<'_>, s: BkeySC<'_, c::Stripe<Validated>>)
    -> Result<(), BchError>
{
    let fs = t.fs();
    fs.event_inc_trace(events::stripe_repair_race, |out| prt!(out, "{}", s.to_text(fs)));

    let opts = get_io_opts(t, None, s.untyped())?;
    update_reconcile_opts(t, None, &opts, iter, 0, s.untyped(),
                          c::set_needs_reconcile_ctx::SET_NEEDS_RECONCILE_other)?;
    t.commit(None, CommitFlags::NO_ENOSPC)
}

/// Too few devices to rebuild @s at its width: start evacuating @need_evacuate
/// of its live data blocks, so their data goes to other stripes and @s narrows
/// to what's left - Retry, to repair it again once the moves are done.
fn stripe_repair_narrow(ctxt: &Moving<'_>, s: BkeySC<'_, c::Stripe<Validated>>, b: StripeBlocks,
                        need_evacuate: u32) -> Result<StripeRepair, BchError> {
    let fs = ctxt.trans().fs();
    let sectors = u64::from(s.v().sectors.get());

    fs.event_inc_trace(events::stripe_repair_narrow, |out| {
        prt!(out, "evacuating {} blocks\n{}", need_evacuate.min(b.live.count_ones()), s.to_text(fs));
    });

    // Blocks on bad devices first: that's the data that has to move, and the
    // stripe narrows to the good blocks. Too few devices for even a one block
    // stripe evacuates every block: the stripe empties, and goes away.
    let blocks = blocks_in(b.live & b.bad).chain(blocks_in(b.live & !b.bad));

    for i in blocks.take(need_evacuate as usize) {
        let ptr = &s.v().block_ptrs()[i];
        let (start, end) = (ptr.offset(), ptr.offset() + sectors);

        if u32::from(ptr.dev()) != c::BCH_SB_MEMBER_INVALID {
            ctxt.evacuate_data(ptr.dev().into(), start, end)?;
        } else {
            ctxt.evacuate_ec_orphan(s.k.p.offset, i as u32, start, end)?;
        }
    }

    Ok(StripeRepair::Retry)
}

/// Rebuild @s as a new stripe of the data blocks in @carry and @move_:
/// repair_rebuild(), which takes over our @h and our read of the
/// stripe, @buf, and hands the new stripe to its task for its writes.
fn stripe_repair_rebuild(ctxt: &Moving<'_>, s: BkeySC<'_, c::Stripe<Validated>>, h: OpenStripe<'_>,
                         buf: KBox<StripeBuf>, devs: c::bch_devs_mask, carry: u32, move_: u32)
    -> Result<StripeRepair, BchError>
{
    repair_rebuild(ctxt, s, h, buf, devs, carry, move_)?;
    Ok(StripeRepair::Done)
}

/// Stripe @s, again, into @key: our own extent updates change its block
/// counts.
fn stripe_repair_reload(t: &TransAttempt<'_, '_>, key: &mut BkeyBuf, s: c::bpos) -> Result<(), BchError> {
    let mut iter = BtreeIter::new(t, c::btree_id::stripes, s, BtreeIterFlags::empty());
    key.reassemble(iter.peek_slot_typed::<c::Stripe>(t)?.untyped());
    Ok(())
}

/// Stripe @s, as read into @buf, can't be reconstructed: rebuild a narrower
/// stripe of the blocks that read clean, in place with new parity, after
/// taking every other block's extents out of the stripe.
fn stripe_repair_unrebuildable(ctxt: &Moving<'_>, h: OpenStripe<'_>, s: BkeySC<'_, c::Stripe<Validated>>,
                               buf: KBox<StripeBuf>, b: StripeBlocks) -> Result<StripeRepair, BchError>
{
    let trans = ctxt.trans();
    let fs = trans.fs();
    let clean = b.live & !buf.failed_mask(PRE);

    // every extent in the blocks, found by backpointer
    trans.write_buffer_flush_sync()?;

    for i in blocks_in(b.live & !clean) {
        drop_block_extents(trans, s, i as u32)?;
    }

    let mut key = BkeyBuf::new();
    lockrestart_do(trans, |t| stripe_repair_reload(t, &mut key, s.k.p))?;
    let s = BkeySC::from(&key).arm::<c::Stripe>().expect("a stripe, as reloaded");
    let b = StripeBlocks::get(fs, s)?;

    if b.live & !clean != 0 {
        let mut msg = log_msg_ratelimited!(fs);
        write!(msg, "stripe repair: extents left in blocks being dropped from an unreconstructable stripe:\n{}",
               s.to_text(fs));
        return Ok(StripeRepair::Retry);
    }

    if b.live == 0 {
        let idx = s.k.p.offset;
        commit_do(trans, None, CommitFlags::NO_ENOSPC, |t| stripe_delete(t, idx, true))?;
        return Ok(StripeRepair::Done);
    }

    let (need_evacuate, devs) = stripe_repair_need_evacuate(fs, s, b);
    if need_evacuate != 0 {
        drop(h);
        return stripe_repair_narrow(ctxt, s, b, need_evacuate);
    }

    // clean blocks on devices that have lost their durability still move
    stripe_repair_rebuild(ctxt, s, h, buf, devs, b.live & !b.bad, b.live & b.bad)
}

/// With @s open, as @h: read it, and decide with what we found.
fn stripe_repair_opened(ctxt: &Moving<'_>, s: BkeySC<'_, c::Stripe<Validated>>, mut b: StripeBlocks,
                        h: OpenStripe<'_>) -> Result<StripeRepair, BchError> {
    let trans = ctxt.trans();
    let fs = trans.fs();

    let (mut buf, read_all) = repair_read(trans, s, b.live)?;

    // Too many blocks bad, or a reconstruct that still doesn't check out:
    // either way, rebuild around the blocks that read clean.
    if read_all && validate_msg(fs, s, &mut buf, true, b.live, c_function_name!()).is_err() {
        return stripe_repair_unrebuildable(ctxt, h, s, buf, b);
    }

    // a block that failed the read is rewritten, not carried
    if read_all {
        b.bad |= buf.failed_mask(PRE);
    }

    let (need_evacuate, devs) = stripe_repair_need_evacuate(fs, s, b);
    if need_evacuate != 0 {
        // narrowing evacuates: the stripe emptying must be able to delete it
        drop(h);
        return stripe_repair_narrow(ctxt, s, b, need_evacuate);
    }

    stripe_repair_rebuild(ctxt, s, h, buf, devs, b.live & !b.bad, b.live & b.bad)
}

/// stripe_repair(), of the blocks @b says - live, and bad.
fn stripe_repair_blocks(ctxt: &Moving<'_>, t: &TransAttempt<'_, '_>, iter: &mut BtreeIter<'_>,
                        s: BkeySC<'_, c::Stripe<Validated>>, b: StripeBlocks) -> Result<StripeRepair, BchError> {
    if b.live == 0 {
        return Ok(StripeRepair::Done);
    }

    // Open the stripe, so the stripe trigger and reuse stay off it once we
    // drop our locks to read it - an open stripe is someone else's to act on,
    // and reconcile will come back to it. Put when we're done, unless the
    // rebuild took it.
    let Some(h) = OpenStripe::tryget_existing(t, iter)? else { return Ok(StripeRepair::Done) };

    stripe_repair_opened(ctxt, s, b, h)
}

/// Repair stripe @s, at @iter, if it's degraded: rebuilt at its width if
/// there are devices for it, narrowed if not, rebuilt around what's left of it
/// if it can't be reconstructed.
///
/// @iter must be intent locked, and @s a copy, not a key in the btree: repair
/// drops the locks to read and write, and runs attempts of its own - and if it
/// did, Done comes back as transaction_restart_nested (relock_or_nested()).
/// Retry ends the attempt as an error does, not relocked: the attempts repair
/// ran are its own, for the caller to say so - restarts_handled().
pub fn stripe_repair(ctxt: &Moving<'_>, t: &TransAttempt<'_, '_>, iter: &mut BtreeIter<'_>,
                     s: BkeySC<'_, c::Stripe<Validated>>) -> Result<StripeRepair, BchError> {
    let b = StripeBlocks::get(t.fs(), s)?;

    if b.bad == 0 {
        stripe_repair_nothing_to_do(t, iter, s)?;
    } else if stripe_repair_blocks(ctxt, t, iter, s, b)? == StripeRepair::Retry {
        return Ok(StripeRepair::Retry);
    }
    t.relock_or_nested()?;
    Ok(StripeRepair::Done)
}

/// Scrub found block @block failing the stripe's checksums: repair @s as if it
/// were degraded, with @block bad. Repair can't find it itself - it may be
/// parity, or not live, and repair only reads the live data blocks.
///
/// # Safety
/// @ctxt C's, in an attempt at @iter, which is intent locked; @s a copy.
#[c_export]
pub unsafe fn bch2_stripe_repair_damaged(ctxt: *mut c::moving_context, iter: &mut BtreeIter<'_>, s: BkeySC<'_>,
                                         block: c_uint) -> c_int {
    // SAFETY: the caller's
    let ctxt = unsafe { Moving::from_raw(ctxt) };
    let t = unsafe { ctxt.trans().attempt_in_progress() };
    let s = s.arm::<c::Stripe>().expect("a stripe");

    // Retry is success, for scrub: narrowing's evacuation, which reconcile
    // finishes
    ret_to_c(StripeBlocks::get(t.fs(), s).and_then(|mut b| {
        b.bad |= 1 << block;
        stripe_repair_blocks(ctxt, &t, iter, s, b).map(|_| ())
    }))
}
// ── Repair: reading the stripe, and rebuilding it ────────────────────────

/// Read @s - which the caller has open - as the fold would: the blocks in
/// @required, then the rest if one of those is bad (the bool). Waits for
/// buffer memory with no btree locks held.
pub(crate) fn repair_read(trans: &BtreeTrans<'_>, s: BkeySC<'_, c::Stripe<Validated>>, required: u32)
    -> Result<(KBox<StripeBuf>, bool), BchError>
{
    let fs = trans.fs();
    let v = s.v();
    let bytes = (usize::from(v.sectors.get()) << 9) * usize::from(v.nr_blocks);

    let mut buf = loop {
        match StripeBuf::new_whole(fs, s, MemWait::Blocked) {
            Err(e) if e.matches(bch_errcode::BCH_ERR_operation_blocked) => {
                trans.unlock_long();
                fs.stripe_buf_mem_wait(bytes);
            }
            ret => break ret?,
        }
    };

    trans.unlock_long();

    read_sync(fs, s, &mut buf, required);
    let read_all = !blocks_good(s, &buf, required);
    if read_all {
        read_sync(fs, s, &mut buf, !required);
    }
    Ok((buf, read_all))
}

/// Rebuild @s as a new stripe of the data blocks in @carry and @move_ - see
/// init_new_stripe_from_old() - taking over the caller's claim @h and its
/// read of the stripe, @read; its task does the writes.
pub(crate) fn repair_rebuild(ctxt: &Moving<'_>, s: BkeySC<'_, c::Stripe<Validated>>, h: OpenStripe<'_>,
                             mut read: KBox<StripeBuf>, devs: c::bch_devs_mask, carry: u32, move_: u32) -> Result<(), BchError> {
    let trans = ctxt.trans();
    let fs = trans.fs();
    let v = s.v();
    let (nr_data, nr_new_data) = (v.nr_data(), (carry | move_).count_ones());

    let b = StripeNew::alloc(fs, devs, c::bch_watermark::BCH_WATERMARK_normal, v.disk_label.into(),
                             v.bits.algorithm().into(), nr_new_data, v.nr_redundant.into(), v.sectors.get().into())
        .map_err(|_| fs.err(bch_errcode::BCH_ERR_ENOMEM_ec_new_stripe_alloc))?;
    let p = NonNull::from(&*b);
    // no head's: in new_stripes from the start
    if let Err((_, b)) = fs.ec.new_stripes.lock().push(b) {
        discard(b);
        return fs.throw(bch_errcode::BCH_ERR_ENOMEM_ec_new_stripe_alloc);
    }
    // SAFETY: in new_stripes, which keeps it till it's discarded or its task
    // frees it
    let ns = unsafe { p.as_ref() };
    // ours, unlaunched: no one else's to wait on, held across the
    // allocation's transactions
    let mut i = ns.lock();

    match Reuse::new(s, h.into_claim()) {
        Ok(r) => i.reuse = Some(r),
        Err(claim) => {
            claim.put(fs);
            drop(i);
            discard(new_stripes_take(fs, p));
            return fs.throw(bch_errcode::BCH_ERR_ENOMEM_ec_new_stripe_alloc);
        }
    }

    init_new_stripe_from_old(&mut i, carry, move_, true);

    stack_pin_init!(let cl = StackClosure::new());

    // A narrower stripe than the one it replaces carries more parity per data
    // block: new space, so reserve it here, where a full filesystem fails with
    // -ENOSPC, rather than NOFAIL once the buckets are ours. A same-width
    // repair replaces parity with as much parity and keeps its NOFAIL
    // reservation below.
    let narrowing = nr_new_data < nr_data;

    let ret = StripeBuf::new_whole(fs, i.new_stripe.stripe(), MemWait::Never)
        .map(|buf| i.new_buf = Some(buf))
        .and_then(|()| if narrowing { i.reserve_parity(ns.fs_ref(), c::bch_reservation_flags::empty()) } else { Ok(()) })
        .and_then(|()| lockrestart_do(trans, |t| stripe_idx_alloc(t, &mut i)));
    if let Err(e) = ret {
        drop(i);
        discard(new_stripes_take(fs, p));
        return Err(e);
    }

    let target = if v.disk_label != 0 { Target::Group(u32::from(v.disk_label) - 1).encode().into() } else { 0 };

    let ret = lockrestart_do(trans, |t| {
        let dev_stripe = dev_stripe_state_get(t, v.disk_label.into())?;
        let mut req = t.alloc_request(target, false, core::ptr::null_mut(), 0, 0,
                                      c::bch_watermark::BCH_WATERMARK_normal, c::bch_write_flags::empty(), cl.as_ptr())?;
        match new_stripe_alloc_buckets(t, &mut req, dev_stripe, &mut i, true) {
            Err(e) if e.matches(bch_errcode::BCH_ERR_operation_blocked) => {
                t.wait_on_allocator(&mut req, e, &cl)?;
                Err(t.restart(bch_errcode::BCH_ERR_transaction_restart_nested))
            }
            ret => ret,
        }
    });

    if let Err(e) = ret {
        let mut msg = log_msg_ratelimited!(fs);
        write!(msg, "stripe repair: error allocating buckets for the new stripe: {e}");
        prt!(&mut msg, "\nold: {}", s.to_text(fs));
        prt!(&mut msg, "\nnew: {}", i.new_stripe.stripe().to_text(fs));
        drop(msg);

        drop(i);
        discard(new_stripes_take(fs, p));
        return Err(e);
    }

    trans.unlock_long();

    if let Err(e) = i.buckets_add(fs) {
        drop(i);
        discard(new_stripes_take(fs, p));
        return Err(e);
    }
    i.allocated = true;

    if !narrowing {
        // NOFAIL: it doesn't fail
        let _ = i.reserve_parity(ns.fs_ref(), c::bch_reservation_flags::BCH_DISK_RESERVATION_NOFAIL);
    }

    // As ec_old_stripe_read(), for an old stripe the caller has already read,
    // with every block we carry good: ec_old_stripe_take().
    old_stripe_carry(&mut i, &mut read);
    let r = i.reuse.as_mut().expect("a rebuild's old stripe");
    r.read = OldRead::Read;
    r.buf = Some(read);

    let v = i.new_stripe.stripe().v();
    let stripe_sectors = u32::from(v.sectors.get()) * u32::from(v.nr_blocks);
    i.write = Some(ctxt.io_start(true, stripe_sectors));
    drop(i);

    // in new_stripes already: no head's
    ns.fill(&fs.ec.new_stripes.lock());
    Ok(())
}
