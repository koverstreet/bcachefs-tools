// SPDX-License-Identifier: GPL-2.0

//! A stripe being made's task: folding in an old stripe's carried blocks,
//! folding its new data blocks into the parity as they're written till it's
//! in flight, creating the stripe, and tearing it down - see stripe_new.rs.

use crate::alloc::capacity::DiskReservation;
use crate::btree::bkey::{BkeyI, BkeySC, BkeyV};
use crate::btree::iter::{commit_do, trans_commit_do, BtreeIter, BtreeIterFlags, CommitFlags, CommitOpts, TransAttempt,
                         UpdateTriggerFlags};
use crate::c;
use crate::data::ec::buf::{blocks_good, blocks_in, generate_block_checksums, generate_checksums, parity_blocks,
                           validate_msg, MemWait, StripeBuf, POST, PRE};
use crate::data::ec::delete::stripe_delete;
use crate::data::ec::io::BlockIo;
use crate::util::refcount::TryRefcount;
use crate::util::gather::Gather;
use crate::data::ec::repair::record_lost_blocks;
use crate::data::ec::stripe::{StripeKey, MAX_BLOCKS};
use crate::data::ec::stripe_new::{new_stripes_take, Internal, Slot, StripeBlock, StripeNew};
use crate::data::ec::stripe_new_reuse::{OldRead, Reuse};
use crate::data::ec::update::{create_watermark, stripe_update_extents};
use crate::errcode::{Errcode, BchError};
use crate::fs::Fs;
use crate::debug::trace::events;
use crate::sb::errors::SbErrorId;
use crate::sb::members::DevIoRef;
use crate::util::alloc::KBox;
use crate::util::rcu::read_lock;
use crate::util::task::{waiter, Resume};
use crate::util::vstructs::Validated;
use crate::{bch_err, btree_trans, fs_inconsistent, inode_opts, logged_ops, prt};
use crate::data_structures::array_vec::ArrayVec;
use core::ptr::NonNull;
use core::sync::atomic::Ordering::SeqCst;
use pin_init::{init, init_zeroed, stack_pin_init, Init};
use crate::format::Btree;
use cstruct_macros::args_stored_once;

// ── Its task ─────────────────────────────────────────────────────────────

/// The old stripe's read, failed: its error, and the carried blocks it cost
/// us - the data the failure lost.
struct OldStripeLost {
    err:    BchError,
    blocks: u32,
}

#[args_stored_once]
pub(crate) async fn stripe_task(fs: &Fs, s: &StripeNew) {
    let at = Resume::queue(&fs.ec.stripe_create_wq);
    stack_pin_init!(let w = Working::new());
    let w = w.get_mut();

    // set before its start, by its head or repair
    let to_read = s.lock().reuse.as_ref().is_some_and(|r| r.read == OldRead::ToRead);
    let fold = if to_read { old_stripe_fold(fs, s, w, at).await } else { Ok(()) };

    // made before it's launched, or the stripe's cancelled and never made
    w.parity = s.lock().parity_buf.take();

    // each new data block folded in as it's written, and freed - and with
    // the writers done, the rest
    loop {
        let seen = s.io_ref_put.snapshot();
        let in_flight = s.in_flight();
        while let Some(i) = next_written(s, w, in_flight) {
            zip_block(fs, s, w, i, at).await;
        }
        if in_flight {
            break;
        }
        s.io_ref_put.wait_async(seen, at).await;
    }
    s.in_flight.wait(at).await;

    // A ref unless writes are dead, not merely stopping: a stripe goes in
    // flight while they're stopped too - open_buckets_stop(), in going
    // read-only's stop_rw(), puts the last buckets of full stripes once
    // c->writes is drained. Then there's none to have, and none needed:
    // stop_rw()'s ec_flush() waits for the stripe.
    let _w = fs.writes.tryget_live();

    let ret = create(fs, s, w, fold, at).await;
    create_done(fs, s, ret);
    // SAFETY: in flight
    unsafe { teardown(s) };
}

/// Only the blocks we carry forward have to be good - everything else in the
/// old stripe is discarded at the fold, parity included, and regenerated at
/// create.
fn old_stripe_required(s: &Internal) -> u32 {
    s.old_block_map().iter().fold(0, |m, &b| m | 1 << b)
}

/// The old stripe's blocks we're carrying forward, in @old, checked out: take
/// them - their buffers moved into the new stripe's, whose carried slots are
/// no writer's, as they're Slot::Carried or Moving.
pub(crate) fn old_stripe_carry(s: &mut Internal, old: &mut StripeBuf) {
    let map = s.old_block_map();
    let new = s.new_buf.as_mut().expect("a new stripe's buffer");
    for (i, &j) in map.iter().enumerate() {
        old.move_block(usize::from(j), new, i);
    }
}

/// Fold the blocks we're carrying forward into the new stripe as soon as the
/// old stripe's read lands, instead of at create: ec_old_stripe_read() and
/// ec_old_stripe_fold(). The carried blocks are Slot::Carried or Moving, so
/// the sector allocator will never hand one to a writer, and the data we
/// swap in can't be overwritten while the rest of the stripe fills.
///
/// Only the carried blocks are read. If one of them is bad, the rest are
/// read so it can be reconstructed: then the old stripe's validated,
/// reconstructing what's bad if it can be, and its blocks carried - or the
/// ones we wanted that are lost said, for create to record damage against.
#[args_stored_once]
async fn old_stripe_fold(fs: &Fs, s: &StripeNew, w: &mut Working, at: Resume<'_>) -> Result<(), OldStripeLost> {
    // The old stripe, its key and buffer, to read with across the awaits;
    // back after
    let required = {
        let mut i = s.lock();
        core::mem::swap(&mut w.reuse, &mut i.reuse);
        old_stripe_required(&i)
    };

    let r = w.reuse.as_deref_mut().expect("a stripe it reuses");
    let buf = r.buf.as_mut().expect("the old stripe's buffer");
    let ret = old_stripe_read(fs, &r.stripe, buf, required, at).await;

    let mut i = s.lock();
    if ret.is_ok() {
        old_stripe_carry(&mut i, buf);
    }
    core::mem::swap(&mut w.reuse, &mut i.reuse);
    ret
}

/// The old stripe's @required blocks, into @buf - and the rest if one's bad,
/// to reconstruct it: whether they're good.
#[args_stored_once]
async fn old_stripe_read(fs: &Fs, old: &StripeKey, buf: &mut StripeBuf, required: u32, at: Resume<'_>)
    -> Result<(), OldStripeLost>
{
    let all = ((1u64 << old.stripe().v().nr_blocks) - 1) as u32;

    let errs = {
        stack_pin_init!(let done = Gather::new());
        let mut io = BlockIo::<_, waiter::Async>::read(fs, buf, &done);
        for i in blocks_in(required) {
            io.submit(old.stripe(), i);
        }
        io.wait(at).await
    };
    errs.record(buf);

    if !blocks_good(old.stripe(), buf, required) {
        let errs = {
            stack_pin_init!(let done = Gather::new());
            let mut io = BlockIo::<_, waiter::Async>::read(fs, buf, &done);
            for i in blocks_in(all & !required) {
                io.submit(old.stripe(), i);
            }
            io.wait(at).await
        };
        errs.record(buf);

        if let Err(err) = validate_msg(fs, old.stripe(), buf, true, required, c"ec_old_stripe_read_done") {
            // Reconstruct either couldn't run - too many failures, so
            // everything that failed to read is lost - or ran and left blocks
            // that still don't check out:
            let e = if err == BchError::from(Errcode::stripe_reconstruct_insufficient_blocks) { PRE } else { POST };
            return Err(OldStripeLost { err, blocks: buf.failed_mask(e) & required });
        }
    }
    Ok(())
}

/// The next new data block that's written - its bucket closed by its last
/// writer, or with @all, its writers done, any left - and not yet folded in.
/// Nothing for a stripe that's cancelled: it's never made.
fn next_written(s: &StripeNew, w: &Working, all: bool) -> Option<usize> {
    let i = s.lock();
    if s.err.get().is_some() || w.parity.is_none() {
        return None;
    }

    (0..i.nr_data() as usize).find(|&j| w.zipped & 1 << j == 0 && match &i.blocks[j] {
        Slot::Bucket(StripeBlock::Claimed(ob)) => all || ob.pin.load(SeqCst) == 0,
        Slot::Bucket(StripeBlock::Pinned(_))   => all,
        _ => false,
    })
}

/// Data block @i, written, folded into the parity and freed - the rest of its
/// bucket zeroed first: zero_out_rest_of_ec_bucket().
#[args_stored_once]
async fn zip_block(fs: &Fs, s: &StripeNew, w: &mut Working, i: usize, at: Resume<'_>) {
    let Working { new_stripe, parity, zipped, .. } = w;
    let parity = parity.as_mut().expect("the parity, checked by next_written()");
    *zipped |= 1 << i;

    let (dev, sectors_free) = {
        let mut l = s.lock();
        let l = &mut *l;
        new_stripe.copy_from(&l.new_stripe);
        let b = l.blocks[i].bucket().expect("a new data block's bucket");
        let buf = l.new_buf.as_mut().expect("a new stripe's buffer");
        if buf.has(i) {
            buf.move_blocks(parity, 1 << i);
        }
        (u32::from(b.id().dev()), b.sectors_free.get())
    };

    // no writer claimed it: made here, by create, which mustn't wait
    if let Err(e) = parity.alloc_block(i, MemWait::Never) {
        s.err.set(e);
        return;
    }

    if sectors_free != 0 {
        let offset = {
            let rcu = read_lock();
            let ca = fs.dev_rcu(dev, &rcu).expect("an open bucket's device");
            u32::from(ca.mi.get().bucket_size) - sectors_free
        };
        let start = (offset as usize) << 9;
        parity.block_mut(i)[start..start + ((sectors_free as usize) << 9)].fill(0);

        let errs = {
            stack_pin_init!(let done = Gather::new());
            let mut io = BlockIo::<_, waiter::Async>::write(fs, parity, &done);
            io.submit_range(new_stripe.stripe(), i, offset, sectors_free);
            io.wait(at).await
        };
        errs.record(parity);
        if parity.failed_mask(PRE) != 0 {
            bch_err!(fs, "error creating stripe: error zeroing the rest of a data bucket");
            s.err.set(fs.err(Errcode::ec_block_write));
        }
    }

    parity.fold_block(new_stripe.stripe(), i);
    generate_block_checksums(&mut s.lock().new_stripe, parity, i);
    parity.free_block(i);
}

/// Whether a device of @v's is being removed.
fn stripe_has_removing_dev(fs: &Fs, v: &c::Stripe<Validated>) -> bool {
    let rcu = read_lock();
    v.block_ptrs().iter().any(|p| fs.dev_rcu(p.dev().into(), &rcu).is_some_and(|ca| ca.removing()))
}

/// The new stripe's key, to commit: its reconcile state set, and no key at
/// its slot already.
fn stripe_key_update(t: &TransAttempt<'_, '_>, k: BkeySC<'_, c::Stripe<Validated>>) -> Result<(), BchError> {
    let fs = t.fs();
    let mut new = t.bkey_make_mut_noupdate(k)?;

    let opts = inode_opts::get_fs(fs, false);
    new.set_needs_reconcile(t, None, &opts, c::SetNeedsReconcileCtx::SET_NEEDS_RECONCILE_foreground, 0)?;

    let mut iter = BtreeIter::new(t, Btree::stripes, k.k.p, BtreeIterFlags::INTENT);
    let old = iter.peek_slot(t)?.expect("a slot always has a key");
    if old.k.type_ != 0 {
        fs_inconsistent!(fs, "error creating stripe: got existing key\n{}", old.to_text(fs));
        return fs.throw(Errcode::EINVAL_ec_stripe_create_existing_key);
    }
    t.update(&iter, new, UpdateTriggerFlags::empty())
}

/// What the task works on: its stripe's keys and buffers, and what create
/// needs of its internal state - out from under its lock, for the IO, which
/// is awaited, and back after. The task's own, made in place: see the file
/// comment.
struct Working {
    new_stripe:      StripeKey,
    new_buf:         Option<KBox<StripeBuf>>,
    /// The parity, the task's from its start
    parity:          Option<KBox<StripeBuf>>,
    /// The new data blocks folded into it
    zipped:          u32,
    reuse:           Option<KBox<Reuse>>,
    res:             Option<DiskReservation<'static>>,
    /// The new blocks moved from the old stripe, to be written
    moving:          c::StripeBlockMask,
    old_block_map:   ArrayVec<u8, MAX_BLOCKS>,
    allocated:       bool,
    watermark:       c::Watermark,
}

impl Working {
    /// Nothing yet: the fold and take() fill it.
    fn new() -> impl Init<Self> {
        init!(Working {
            new_stripe      <- StripeKey::empty(),
            new_buf:        None,
            parity:         None,
            zipped:         0,
            reuse:          None,
            res:            None,
            moving          <- init_zeroed(),
            old_block_map:  ArrayVec::new(),
            allocated:      false,
            watermark       <- init_zeroed(),
        })
    }

    /// @i's, out from under its lock: its keys copied, and what it says of
    /// them; its buffers and reservation swapped in.
    fn take(&mut self, i: &mut Internal) {
        self.new_stripe.copy_from(&i.new_stripe);
        self.moving          = Default::default();
        for (j, b) in i.blocks.iter().enumerate() {
            if b.moving() {
                self.moving.set(j);
            }
        }
        self.old_block_map   = i.old_block_map();
        self.allocated       = i.allocated;
        self.watermark       = i.watermark;
        self.swap(i);
    }

    /// Back to @i: the new key, as create edited it, and the buffers and
    /// what's left of the reservation.
    fn put_back(&mut self, i: &mut Internal) {
        i.new_stripe.copy_from(&self.new_stripe);
        self.swap(i);
    }

    fn swap(&mut self, i: &mut Internal) {
        core::mem::swap(&mut self.new_buf, &mut i.new_buf);
        core::mem::swap(&mut self.reuse, &mut i.reuse);
        core::mem::swap(&mut self.res, &mut i.res);
    }
}

/// Data buckets of the new stripe all written: create the stripe, with write
/// refs on the devices of the blocks it writes.
#[args_stored_once]
async fn create(fs: &Fs, s: &StripeNew, w: &mut Working, fold: Result<(), OldStripeLost>, at: Resume<'_>)
    -> Result<(), BchError>
{
    w.take(&mut s.lock());
    let ret = create_iorefs(fs, s, w, fold, at).await;
    w.put_back(&mut s.lock());
    ret
}

async fn create_iorefs(fs: &Fs, s: &StripeNew, w: &mut Working, fold: Result<(), OldStripeLost>,
                       at: Resume<'_>) -> Result<(), BchError> {
    // What's written: the data blocks new to this stripe, the ones moving, and
    // the parity. A block carried forward in place isn't, and its device may
    // be read-only, or offline for now - it still holds the block, durably.
    // Device removal is fenced by ->removing and fs_ec_flush_outstanding(),
    // not by these refs.
    //
    // Kept by device, not by c->devs[idx]: dev_remove may clear that slot
    // while our io ref still pins the device.
    let carried = |i: usize| i < w.old_block_map.len() && !w.moving.test(i);
    let mut cas: ArrayVec<DevIoRef<c::DevWriteRefKind>, MAX_BLOCKS> = ArrayVec::new();
    for (i, ptr) in w.new_stripe.stripe().v().block_ptrs().iter().enumerate() {
        if carried(i) {
            continue;
        }
        let Some(ca) = fs.dev_get_ioref::<c::DevWriteRefKind>(ptr.dev().into()) else {
            return fs.throw(Errcode::stripe_create_device_offline);
        };
        cas.push(ca);
    }

    create_with_iorefs(fs, s, w, fold, at).await
}

#[args_stored_once]
async fn create_with_iorefs(fs: &Fs, s: &StripeNew, w: &mut Working, fold: Result<(), OldStripeLost>,
                            at: Resume<'_>) -> Result<(), BchError>
{
    if let Some(e) = s.err.get() {
        if !e.matches(c::EROFS) {
            bch_err!(fs, "error creating stripe: error writing data buckets");
        }
        return Err(e);
    }

    // bch2_dev_remove() sets ->removing before its data drop, then flushes
    // outstanding creates, so checking before we commit is enough. Only the
    // new key's pointers matter: blocks being moved off the device get fresh
    // buckets, and reading it during the drop is how their data moves. What
    // must not commit is a pointer copied while the device was healthy - a
    // reused stripe's - after the removal walk has passed it.
    if stripe_has_removing_dev(fs, w.new_stripe.stripe().v()) {
        return fs.throw(Errcode::stripe_create_device_removing);
    }

    let (nr_data, nr_blocks) = {
        let v = w.new_stripe.stripe().v();
        (v.nr_data() as usize, usize::from(v.nr_blocks))
    };
    let Working { new_stripe, new_buf, parity, reuse, moving, old_block_map, allocated, watermark, res, .. } = w;

    // The fold was awaited before we were in flight: its error is settled.
    // The reuse is abandoned, so the old stripe stays as it is and nothing
    // else discovers the loss until someone reads one of those files:
    if let Err(lost) = &fold {
        // Too few blocks to reconstruct: the lost blocks' buffers still hold
        // what we read, and an extent that checks out against it isn't lost
        // - a block fails on one bad granule. After a reconstruct they hold
        // its output instead, which says nothing about what's on disk.
        let as_read = lost.err == BchError::from(Errcode::stripe_reconstruct_insufficient_blocks);
        let trans = btree_trans!(fs);
        let r = reuse.as_ref().expect("a fold's old stripe");
        record_lost_blocks(&trans, r.stripe.stripe(), lost.blocks,
                           SbErrorId::stripe_reconstruct_failed, false,
                           r.buf.as_deref().filter(|_| as_read));
        return Err(lost.err);
    }

    assert!(*allocated, "creating stripe {}, not allocated", new_stripe.idx());

    // the new blocks are in: what's left is the old stripe's
    let buf = new_buf.as_mut().expect("a new stripe's buffer");
    parity.as_mut().expect("a new stripe's parity").move_blocks(buf, parity_blocks(new_stripe.stripe()));
    for i in 0..old_block_map.len() {
        buf.fold_block(new_stripe.stripe(), i);
    }
    generate_checksums(new_stripe, buf);

    // the data blocks that moved, and p/q
    let errs = {
        stack_pin_init!(let done = Gather::new());
        let mut io = BlockIo::<_, waiter::Async>::write(fs, buf, &done);
        for i in (0..old_block_map.len()).filter(|&i| moving.test(i)) {
            io.submit(new_stripe.stripe(), i);
        }
        for i in nr_data..nr_blocks {
            io.submit(new_stripe.stripe(), i);
        }
        io.wait(at).await
    };
    errs.record(buf);

    if buf.failed_mask(PRE) != 0 {
        bch_err!(fs, "error creating stripe: error writing redundancy buckets");
        return fs.throw(Errcode::ec_block_write);
    }

    let new = new_stripe.stripe();
    let old_idx = reuse.as_ref().map_or(0, |r| r.stripe.idx());
    let new_idx = new_stripe.idx();

    let mut op = BkeyI::<c::LoggedOpStripeUpdate>::new();
    op.v.old_idx       = old_idx.into();
    op.v.new_idx       = new_idx.into();
    op.v.old_blocks_nr = old_block_map.len() as u8;
    op.v.old_block_map[..old_block_map.len()].copy_from_slice(old_block_map);

    let watermark = create_watermark(*watermark);
    let opts = CommitOpts::new().watermark(watermark).flags(CommitFlags::NO_CHECK_RW | CommitFlags::NO_ENOSPC);

    let res = match res.take() {
        Some(res) => res,
        None      => DiskReservation::new(fs),
    };
    trans_commit_do(fs, Some(&res), opts, |t| {
        stripe_key_update(t, new)?;
        logged_ops::start(t, op.as_mut())
    })?;

    let trans = btree_trans!(fs);
    let map = &old_block_map[..];
    // no old stripe, if it isn't a reuse: nothing's carried from it
    let old = match reuse {
        Some(r) if !map.is_empty() => r.stripe.stripe(),
        _ => new,
    };
    let ret = stripe_update_extents(&trans, old, new, map, watermark);
    logged_ops::finish(&trans, op.as_ref(), ret, CommitOpts::new().watermark(watermark))?;

    // The trigger and the delete worker skip open stripes, and both of ours
    // may have emptied while we held them:
    commit_do(&trans, None, opts, |t| {
        if old_idx != 0 && old_idx != new_idx {
            stripe_delete(t, old_idx, true)?;
        }
        stripe_delete(t, new_idx, true)
    })
}

/// What creating @s came to, counted and traced.
fn create_done(fs: &Fs, s: &StripeNew, ret: Result<(), BchError>) {
    let i = s.lock();
    let new = i.new_stripe.stripe();
    match ret {
        Err(e) => {
            s.err.set(e);
            fs.event_inc_trace(events::stripe_create_fail, |out| {
                prt!(out, "error {e}\n{}", new.to_text(fs));
            });
        }
        Ok(()) if i.reuse.is_some() => fs.event_inc_trace(events::stripe_reuse, |out| {
            let ov = i.reuse.as_ref().expect("just seen").stripe.stripe();
            prt!(out, "Reused {}/{} data blocks\n", i.old_block_map().len(), ov.v().nr_data());
            prt!(out, "\nOld: {}", ov.to_text(fs));
            prt!(out, "\nNew: {}", new.to_text(fs));
        }),
        Ok(()) => fs.event_inc_trace(events::stripe_create, |out| {
            prt!(out, "\n{}", new.to_text(fs));
        }),
    }
}

/// What it holds given back - its writers done: the blocks they claimed
/// closed, the rest put; a repair's writes done with it. Out of new_stripes
/// it goes in the task's done.
///
/// # Safety
/// It's in flight: every writer's io ref put, so nothing else has it.
unsafe fn teardown(s: &StripeNew) {
    s.release();
}

/// The task's done: the stripe out of new_stripes and freed, its task with
/// it, then the flushes woken.
///
/// # Safety
/// @s is the stripe of a task that's done.
pub(crate) unsafe fn stripe_done(s: NonNull<StripeNew>) {
    // the filesystem outlives the stripe's box: see stripe_new.rs
    let fs: *const Fs = &**unsafe { s.as_ref() }.fs_ref();
    let fs = unsafe { &*fs };
    drop(new_stripes_take(fs, s));
    fs.ec.new_stripes_changed.wake();
}
