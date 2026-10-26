// SPDX-License-Identifier: GPL-2.0

//! Making stripes - ec/create.c's, with stripe_new.rs, which has a stripe
//! being made, from its head's staging to its creation: here, which devices a
//! stripe can use, the widen cache - for reconcile's check pass and its
//! stripes scan - the stripe heads, a new stripe's buckets, packing a new
//! stripe into an old one, a new stripe's slot and allocation, stripe
//! deletion, walking a stripe block's extents - to point them at a new
//! stripe, to record their loss, or to take them out of the stripe - and what
//! stripe repair decides to do.
//!
//! A stripe's devices are its disk label's, 0 for every device's: two views
//! of them. disk_label_ec_devs() is what a stripe can be made on now - rw
//! for user data, durable, with one bucket size, the most common - which is
//! what stripe heads allocate from and what repair has to work with.
//! disk_label_ec_rw_member_devs() is the configured view, online or not,
//! that a stripe's can_widen is measured against: so a device going offline
//! and coming back doesn't have reconcile rewrite every stripe's.

use crate::alloc::backpointers::{backpointer_get_key, BackpointerPos};
use crate::alloc::buckets::DiskReservation;
use crate::alloc::disk_groups::Target;
use crate::alloc::lru::{LruId, LruPos};
use crate::btree::bkey::prelude::*;
use crate::btree::bkey::{bkey_gt, bpos_max, pos, BkeyI, BkeySC, POS_MIN};
use crate::btree::bkey_buf::BkeyBuf;
use crate::btree::iter::{commit_do, lockrestart_do, BtreeIter, BtreeIterFlags, BtreeTrans, CommitFlags, CommitOpts,
                         LoopFlow, TransAttempt, UpdateTriggerFlags, Watermark, is_restart};
use crate::btree::write_buffer::WbMaybeFlush;
use crate::c;
use crate::c::bch_errcode;
use crate::data::copygc;
use crate::data::durability::drop_extra_durability;
use crate::data::ec::io::{blocks_in, validate_msg, StripeBuf, StripeKey, PRE};
use crate::data::ec::stripe_new::{self, Internal, StripeBlock, StripeNew};
use crate::data::ec::trigger::{ec_stripe_mem_alloc, ptr_in_block, OpenStripe, STRIPE_LRU_POS_EMPTY};
use crate::data::moving::Moving;
use crate::data::ptrs::{brand, extent_nonce, BkeyVPtrs, BkeyVPtrsMut, ExtentEntry, Ptrs, PtrsMut};
use crate::data::reconcile::needs_reconcile::update_reconcile_opts;
use crate::errcode::{ret_to_c, BchError};
use crate::fs::Fs;
use crate::init::damage;
use crate::init::error::id;
use crate::inode_opts::get_io_opts;
use crate::locking::Guard;
use crate::logged_ops;
use crate::sb::counters::events;
use crate::types::Align8U64;
use crate::util::alloc::flags::GFP_KERNEL;
use crate::util::alloc::{kbox_pin_init, KVVec};
use crate::util::backref::BackRef;
use crate::util::ffi::Opaque;
use crate::util::log::LogMsg;
use crate::util::place::Placed;
use crate::util::rcu::{read_lock, ReadGuard};
use crate::util::vstructs::Validated;
use crate::util::bitmap::Bitmap;
use crate::util::Printbuf;
use crate::{bch_err, btree_trans, c_function_name, fs_inconsistent, fsck_err_flags, log_msg_ratelimited, new_lock, prt};
use core::ffi::{c_int, c_uint};
use core::mem::size_of;
use core::ops::{ControlFlow, RangeInclusive};
use core::sync::atomic::Ordering;
use cstruct_macros::c_export;
use pin_init::pin_init;

// ── A stripe's devices ───────────────────────────────────────────────────

/// Disk label @disk_label's target: none for 0.
fn disk_label_target(disk_label: u32) -> Target {
    if disk_label != 0 { Target::Group(disk_label - 1) } else { Target::Null }
}

impl Fs {
    /// The most common bucket size among @devs - on a tie, the smaller:
    /// pick_blocksize().
    fn ec_pick_blocksize(&self, devs: &c::bch_devs_mask, rcu: &ReadGuard) -> u32 {
        let sizes = || self.devs_rcu_in(devs.d.iter(), rcu).map(|ca| u32::from(ca.mi.bucket_size));

        sizes()
            .map(|size| (sizes().filter(|&s| s == size).count(), size))
            .fold((0, 0), |best, (nr, size)| {
                if nr > best.0 || (nr == best.0 && size < best.1) { (nr, size) } else { best }
            })
            .1
    }

    /// The devices a stripe in @disk_label can be made on now: rw for user
    /// data, durable, with buckets of @blocksize - or if that's 0, of the
    /// most common size among them - and the blocksize.
    pub fn disk_label_ec_devs(&self, disk_label: u32, blocksize: u32) -> (c::bch_devs_mask, u32) {
        let mut devs = self.target_rw_devs(c::bch_data_type::BCH_DATA_user, disk_label_target(disk_label));
        let rcu = read_lock();

        let all = devs;
        for ca in self.devs_rcu_in(all.d.iter(), &rcu).filter(|ca| ca.mi.durability == 0) {
            devs.d.clear(ca.dev_idx.into());
        }

        let blocksize = if blocksize != 0 { blocksize } else { self.ec_pick_blocksize(&devs, &rcu) };

        let all = devs;
        for ca in self.devs_rcu_in(all.d.iter(), &rcu).filter(|ca| u32::from(ca.mi.bucket_size) != blocksize) {
            devs.d.clear(ca.dev_idx.into());
        }
        (devs, blocksize)
    }

    /// Whether @devs - disk_label_ec_devs()'s, for @disk_label - can take a
    /// stripe of @redundancy parity blocks: redundancy + 2 devices, not + 1 -
    /// one data block plus parity is worse than replication - in as many
    /// failure domains, which count separately as the allocator won't put two
    /// blocks in one. Why not, into @out: callers park work on this answer.
    pub fn ec_devs_sufficient(&self, devs: &c::bch_devs_mask, disk_label: u32, redundancy: u32,
                              out: Option<&mut Printbuf>) -> bool {
        let nr_devs = devs.d.weight();
        let nr_domains = self.target_nr_domains(devs);
        let need = redundancy + 2;

        if nr_devs >= need && nr_domains >= need {
            return true;
        }

        if let Some(out) = out {
            write!(out, "disk_label {disk_label}: {nr_devs} devs, {nr_domains} domains, need {need} of each\n\
                         (bch2_disk_label_ec_devs() drops durability 0, and every\n\
                         device whose bucket size isn't the most common one)");
        }
        false
    }

    /// Whether a stripe of @redundancy parity blocks can be made in @target
    /// now - for reconcile, so it doesn't queue EC work that can't progress,
    /// which would spin requeueing data_update_fail. Why not, into @trace.
    pub fn can_form_ec_stripe(&self, target: u32, redundancy: u32, mut trace: Option<&mut Printbuf>) -> bool {
        if redundancy == 0 {
            if let Some(t) = trace.as_deref_mut() {
                write!(t, "redundancy 0");
            }
            return false;
        }

        let disk_label = match Target::decode(target) {
            // A group past U8_MAX can't be a disk label, and the stripe head
            // lookup refuses it outright: folding it to disk label 0 would ask
            // about every device, and say yes to a target the allocator won't
            // serve
            Target::Group(g) if g > u8::MAX.into() => {
                if let Some(t) = trace.as_deref_mut() {
                    write!(t, "target group {g} > U8_MAX");
                }
                return false;
            }
            Target::Group(g) => g + 1,
            _                => 0,
        };

        let (devs, _) = self.disk_label_ec_devs(disk_label, 0);
        self.ec_devs_sufficient(&devs, disk_label, redundancy, trace)
    }

    /// The devices in @disk_label configured rw - online or not - durable,
    /// taking user data, with buckets of @blocksize: what a stripe's can_widen
    /// is measured against.
    ///
    /// As C's, a label whose group is gone counts every device, as label 0.
    pub fn disk_label_ec_rw_member_devs(&self, disk_label: u32, blocksize: u32) -> c::bch_devs_mask {
        let rcu = read_lock();
        let mut devs = c::bch_devs_mask::default();

        let label = match disk_label {
            0 => None,
            l => self.target_to_mask(disk_label_target(l), &rcu),
        };
        let in_label = |ca: &&c::bch_dev| label.is_none_or(|t| t.d.test(ca.dev_idx.into()));

        for ca in self.devs_rcu_in(0..self.nr_devices() as usize, &rcu).filter(in_label) {
            if ca.rw_durability() != 0 &&
               ca.mi.data_allowed & (1 << c::bch_data_type::BCH_DATA_user.0) != 0 &&
               u32::from(ca.mi.bucket_size) == blocksize
            {
                devs.d.set(ca.dev_idx.into());
            }
        }
        devs
    }

    /// How many disk_label_ec_rw_member_devs() there are.
    pub fn ec_rw_member_devs_nr(&self, disk_label: u8, sectors: u16) -> u32 {
        self.disk_label_ec_rw_member_devs(disk_label.into(), sectors.into()).d.weight()
    }
}

// ── The widen cache ──────────────────────────────────────────────────────

/// How many RW members each (disk_label, sectors) has - the widening target
/// a stripe there has - looked up once each: C's widen_cache. Sorted by
/// (disk_label, sectors).
#[derive(Default)]
pub struct WidenCache(KVVec<((u8, u16), u32)>);

impl WidenCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// @disk_label's RW members, for a stripe of @sectors.
    pub fn lookup(&mut self, fs: &Fs, disk_label: u8, sectors: u16) -> Result<u32, BchError> {
        let key = (disk_label, sectors);
        match self.0.binary_search_by_key(&key, |e| e.0) {
            Ok(i) => Ok(self.0[i].1),
            Err(i) => {
                let nr = fs.ec_rw_member_devs_nr(disk_label, sectors);
                self.0.push((key, nr), GFP_KERNEL)?;
                self.0[i..].rotate_right(1);
                Ok(nr)
            }
        }
    }

    /// The can_widen stripe @s should have now: how many more data blocks
    /// its disk label's RW members would let it have.
    pub fn can_widen(&mut self, fs: &Fs, s: &c::Stripe) -> Result<u8, BchError> {
        let nr_devs = self.lookup(fs, s.disk_label, s.sectors.get())?;
        Ok(s.want_can_widen(nr_devs, fs.opts().ec_max_data_blocks.into()))
    }
}

// ── Stripe heads ─────────────────────────────────────────────────────────
//
// The sector allocator gets erasure coded buckets from a stripe head - one
// per disk label, algorithm, redundancy and watermark - which stages the
// stripe being filled. The head is locked from lookup until
// bch2_ec_stripe_head_put(): bch2_ec_stripe_head_get() - stripe_new.rs's -
// works under the lock between the two - allocating the stripe, its buffers,
// claiming a bucket of it - and carries it held to C's caller
// (Guard::leave_locked()), until the sector allocator converts.
//
// A head hands its stripe on - pending, to the create path - when every data
// block's claimed, when its devices change under a stripe not yet allocated,
// or when allocation stops: on the filesystem, or a device, going read-only.

pub(crate) type HeadGuard<'f> = Guard<'f, c::mutex, c::HeadState>;

/// The stripe @g is filling, if it has one.
fn head_stripe<'g>(g: &'g HeadGuard<'_>) -> Option<&'g StripeNew> {
    g.s.as_deref()
}

/// Hand the stripe @g is filling on to the create path, as it is, with
/// error @err if it's cancelled.
fn stripe_head_detach(fs: &Fs, g: &mut HeadGuard<'_>, err: Option<BchError>) {
    let s = g.s.take().expect("detaching a stripe head's stripe, with none");
    if let Some(err) = err {
        s.err.store(ret_to_c(Err(err)), Ordering::Relaxed);
    }
    stripe_new::set_pending(fs, s);
}

/// @h's devices, after the allocator's RW devices changed: a stripe not yet
/// allocated that some have left is cancelled.
fn stripe_head_devs_update(fs: &Fs, h: &c::ec_stripe_head, g: &mut HeadGuard<'_>) {
    let old = g.devs;
    (g.devs, g.blocksize) = fs.disk_label_ec_devs(h.disk_label, 0);
    g.nr_active_devs = g.devs.d.weight();
    g.insufficient_devs = !fs.ec_devs_sufficient(&g.devs, h.disk_label, h.redundancy, None);

    let mut leaving = old;
    leaving.d.andnot(&g.devs.d);
    if head_stripe(g).is_some_and(|s| !s.lock().allocated) && !leaving.d.is_empty() {
        stripe_head_detach(fs, g, Some(BchError::from(c::EINTR)));
    }
}

/// The head for @disk_label, @algo, @redundancy and @watermark - made if
/// there's none - locked, its devices brought up to date: or the error that
/// stops erasure coding for it, the filesystem going read-only or the label
/// too few devices.
pub(crate) fn stripe_head_get<'f>(t: &TransAttempt<'_, 'f>, disk_label: u32, algo: u32, redundancy: u32,
                       watermark: c::bch_watermark) -> Result<(&'f c::ec_stripe_head, HeadGuard<'f>), BchError> {
    let fs = t.fs();
    if redundancy == 0 {
        return fs.throw(bch_errcode::BCH_ERR_ec_alloc_failed_no_redundancy);
    }
    // SAFETY: boxed - pinned - and freed only with @fs
    let fs_lifetime = |h: &c::ec_stripe_head| -> &'f c::ec_stripe_head { unsafe { &*core::ptr::from_ref(h) } };

    let mut heads = fs.ec.stripe_heads.lock_trans(t)?;
    if fs.flag(c::bch_fs_flags::BCH_FS_going_ro) {
        return fs.throw(bch_errcode::BCH_ERR_erofs_no_writes);
    }

    let found = heads.iter()
        .find(|h| h.disk_label == disk_label && h.algo == algo && h.redundancy == redundancy &&
                  h.watermark == watermark)
        .map(|h| fs_lifetime(h));

    let (h, mut g) = match found {
        Some(h) => {
            // Not under the list's lock: a writer waiting for one head would
            // hold up every other head's writers behind it.
            drop(heads);
            let g = h.lock.lock_trans(t)?;
            // bch2_fs_ec_stop() may have been through this head while we
            // waited: it sets going_ro before it starts, so anything we
            // started now would be missed
            if fs.flag(c::bch_fs_flags::BCH_FS_going_ro) {
                return fs.throw(bch_errcode::BCH_ERR_erofs_no_writes);
            }
            (h, g)
        }
        None => {
            let new = kbox_pin_init(pin_init!(c::ec_stripe_head {
                lock <- new_lock!(c::HeadState::default()),
                disk_label,
                algo,
                redundancy,
                watermark,
            }? BchError));
            let pushed = new.and_then(|h| Ok(heads.push(h, GFP_KERNEL)?));
            if pushed.is_err() {
                return fs.throw(bch_errcode::BCH_ERR_ENOMEM_stripe_head_alloc);
            }
            let h = fs_lifetime(heads.last().expect("just pushed"));
            // taken before the list's lock is let go: no one's seen it yet
            let g = h.lock.try_lock().expect("a new stripe head, locked");
            drop(heads);
            (h, g)
        }
    };

    let count = fs.allocator.rw_devs_change_count();
    if g.rw_devs_change_count != count {
        stripe_head_devs_update(fs, h, &mut g);
        g.rw_devs_change_count = count;
    }

    if g.insufficient_devs {
        return fs.throw(bch_errcode::BCH_ERR_ec_alloc_failed_insufficient_devs);
    }
    Ok((h, g))
}

/// Unlock the head @g locks, handing on its stripe if every data block's been
/// claimed: nothing more can go into it.
pub(crate) fn stripe_head_put(fs: &Fs, mut g: HeadGuard<'_>) {
    let full = head_stripe(&g).is_some_and(|s| {
        let s = s.lock();
        s.allocated && (0..s.nr_data() as usize).all(|i| s.blocks_allocated.test(i))
    });
    if full {
        stripe_head_detach(fs, &mut g, None);
    }
}

/// stripe_head_put(), for C.
///
/// # Safety
/// @h locked, by bch2_ec_stripe_head_get().
#[c_export]
pub unsafe fn bch2_ec_stripe_head_put(c: &Opaque<c::bch_fs>, h: &c::ec_stripe_head) {
    // SAFETY: the caller's
    stripe_head_put(Fs::from_c(c), unsafe { h.lock.assume_locked() })
}

/// Whether stopping device @ca - None: the filesystem - cancels @s: one with a
/// block on it, freshly allocated or carried from a reused stripe.
/// should_cancel_stripe().
fn stripe_on_dev(s: &Internal, ca: Option<&c::bch_dev>) -> bool {
    let Some(ca) = ca else { return true };
    let v = s.new_stripe.stripe().v();

    (0..usize::from(v.nr_blocks)).any(|i| {
        let ob_dev = s.blocks[i].as_ref().map(|b| b.dev());
        // a reused block isn't in blocks[]: init_new_stripe_from_old()
        // copies the old pointer into the key
        ob_dev == Some(ca.dev_idx.into()) ||
            (s.blocks_allocated.test(i) && v.block_ptrs()[i].dev() == ca.dev_idx)
    })
}

/// Cancel the stripes the heads are filling with a block on @ca - every one,
/// for None: for the filesystem going read-only, which it's already set, so
/// heads stay empty after.
fn stripe_heads_stop(fs: &Fs, ca: Option<&c::bch_dev>) {
    let heads = fs.ec.stripe_heads.lock();
    for h in heads.iter() {
        let mut g = h.lock.lock();
        if head_stripe(&g).is_some_and(|s| stripe_on_dev(&s.lock(), ca)) {
            stripe_head_detach(fs, &mut g, Some(BchError::from(bch_errcode::BCH_ERR_erofs_no_writes)));
        }
    }
}

#[c_export]
pub fn bch2_ec_stop_dev(c: &Opaque<c::bch_fs>, ca: &c::bch_dev) {
    stripe_heads_stop(Fs::from_c(c), Some(ca))
}

#[c_export]
pub fn bch2_fs_ec_stop(c: &Opaque<c::bch_fs>) {
    stripe_heads_stop(Fs::from_c(c), None)
}

/// The recovery pass that read the stripes into memory: there's nothing to
/// read now, but passes keep their numbers.
#[c_export]
pub fn bch2_stripes_read(_c: &Opaque<c::bch_fs>) -> c_int {
    0
}

/// At shutdown, free the heads: no stripe's blocks are still allocated - and
/// with them, the last stripes being made.
#[c_export]
pub fn bch2_fs_ec_exit(c: &Opaque<c::bch_fs>) {
    let fs = Fs::from_c(c);
    let heads = core::mem::take(&mut *fs.ec.stripe_heads.lock());

    for h in heads {
        let mut g = h.lock.lock();
        if let Some(s) = head_stripe(&g) {
            let s = s.lock();
            let nr_blocks = usize::from(s.new_stripe.stripe().v().nr_blocks);
            assert!(s.blocks[..nr_blocks].iter().all(Option::is_none),
                    "shutting down with a stripe head's stripe holding buckets");
            drop(s);
            stripe_new::head_stripe_free(fs, g.s.take().expect("just seen"));
        }
    }
    assert!(fs.ec.new_stripes.lock().is_empty(), "shutting down with stripes still being made, or room kept for them");
}

/// Each head's key and the stripe it's filling, for new_stripes_to_text().
pub(crate) fn stripe_heads_to_text(out: &mut Printbuf, fs: &Fs) {
    let heads = fs.ec.stripe_heads.lock();
    for h in heads.iter() {
        let g = h.lock.lock();
        prt!(out, "disk label {} algo {} redundancy {} {} nr created {}:\n",
             h.disk_label, h.algo, h.redundancy, h.watermark.name(), g.nr_created);
        if let Some(s) = head_stripe(&g) {
            stripe_new::new_stripe_to_text(out, fs, s);
        }
    }
}

// ── A disk label's striping ──────────────────────────────────────────────

/// Disk label @disk_label's striping of new stripes' buckets, made if it has
/// none yet: ec_dev_stripe_state_get(). It's freed only with the filesystem,
/// so it's the filesystem's to borrow - unlocked: its lock is only for the
/// allocator's rotation through the devices, which new_stripe_alloc_buckets()
/// takes it around.
pub(crate) fn dev_stripe_state_get<'f>(t: &TransAttempt<'_, 'f>, disk_label: u32)
    -> Result<&'f c::ec_dev_stripe_state, BchError>
{
    let fs = t.fs();
    let mut states = fs.ec.dev_stripe_states.lock_trans(t)?;

    let s = match states.iter().find(|s| s.disk_label == disk_label) {
        Some(s) => s,
        None => {
            let s = kbox_pin_init(pin_init!(c::ec_dev_stripe_state {
                lock <- new_lock!(c::DevStripes::default()),
                disk_label,
            }? BchError))?;
            states.push(s, GFP_KERNEL)?;
            states.last().expect("just pushed")
        }
    };
    // SAFETY: boxed - pinned - and freed only with @fs
    Ok(unsafe { &*core::ptr::from_ref::<c::ec_dev_stripe_state>(s) })
}

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
        for ob in req.ptrs.take(fs) {
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
    let mut failed = s.devs;
    failed.d.andnot(&req.devs_may_alloc.d);
    req.devs_may_alloc = failed;

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
    let saved = (req.flags, req.data_type, req.ptrs, req.nr_replicas, req.nr_effective, req.devs_may_alloc);

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

    (req.flags, req.data_type, req.ptrs, req.nr_replicas, req.nr_effective, req.devs_may_alloc) = saved;
    req.target_frac = 0;
    ret
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
// the new stripe, by the new stripe's task: stripe_new.rs.

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

// ── Stripe deletion ──────────────────────────────────────────────────────
//
// A stripe is deleted once it's empty, and not while it's open - held by a
// stripe being created, or by repair. The stripe trigger deletes one that
// empties while it isn't; the holders delete theirs as they let go; and the
// worker deletes whatever empty stripes the stripe lru still has.

/// Delete stripe @idx if it's empty: ec_stripe_delete(). @is_open: the
/// caller holds it open, so it must exist.
pub(crate) fn stripe_delete(t: &TransAttempt<'_, '_>, idx: u64, is_open: bool) -> Result<(), BchError> {
    let fs = t.fs();
    let mut iter = BtreeIter::new(t, c::btree_id::stripes, pos(0, idx), BtreeIterFlags::INTENT);
    let k = iter.peek_slot(t)?.expect("a slot");

    // checked with the stripe intent locked: an existing stripe is opened
    // only under that lock - bch2_stripe_tryget_existing() - so a
    // create can't open it, and load the key we're deleting, before we commit
    if !is_open && fs.stripe_is_open(idx) {
        return Ok(());
    }

    // the worker races with the write buffer: its lru entry can outlive the
    // stripe. An open stripe must exist, but needn't be empty.
    let Some(s) = k.arm::<c::Stripe>() else {
        if is_open {
            fs_inconsistent!(fs, "error deleting stripe: open stripe {idx} missing\n{}", k.to_text(fs));
        }
        return Ok(());
    };

    if s.v().lru_pos() != STRIPE_LRU_POS_EMPTY {
        return Ok(());
    }

    fs.event_inc_trace(events::stripe_delete, |out| prt!(out, "{}", k.to_text(fs)));
    t.delete_at(&iter, UpdateTriggerFlags::empty())
}

/// Deletes empty stripes that aren't open. They sit at lru time
/// STRIPE_LRU_POS_EMPTY, ahead of every reusable one: it walks just those.
///
/// XXX: can we kill this and delete stripes from the trigger?
pub(crate) fn delete_empty_stripes(ec: &c::bch_fs_ec) {
    let fs = ec.container();
    let trans = btree_trans!(fs);
    let at = |offset| LruPos::new(LruId::Stripes.id(), STRIPE_LRU_POS_EMPTY, offset).to_bpos();

    // as C's: errors - going read-only - end the walk, and are dropped
    let _ = trans.write_buffer_tryflush().and_then(|_| {
        BtreeIter::new(&trans, c::btree_id::lru, at(0), BtreeIterFlags::empty())
            .for_each_max_commit(&trans, at(u64::MAX), BtreeIterFlags::empty(), None,
                                 CommitFlags::NO_ENOSPC, |t, _, k| stripe_delete(t, k.k.p.offset, false))
    });
}

impl Fs {
    /// Kick the worker that deletes empty stripes, unless the filesystem's
    /// going read-only: bch2_do_stripe_deletes().
    pub fn do_stripe_deletes(&self) {
        self.ec.stripe_delete_work.queue(self.write_ref_wq());
    }
}

#[c_export]
pub fn bch2_do_stripe_deletes(c: &Opaque<c::bch_fs>) {
    Fs::from_c(c).do_stripe_deletes()
}

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
type Extent<'k, 'a> = Option<(&'k BtreeIter<'a>, BkeySC<'k>)>;

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
fn block_for_each_extent<'t, F>(trans: &'t BtreeTrans<'t>, s: BkeySC<'_, c::Stripe<Validated>>, block: u32,
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

/// Block @old_blocknr of @old, to be block @new_blocknr of @new - the same
/// stripe, for a new one's own blocks.
struct BlockUpdate<'o, 'n> {
    old:         BkeySC<'o, c::Stripe<Validated>>,
    new:         BkeySC<'n, c::Stripe<Validated>>,
    old_block:   c::bch_extent_ptr,
    new_block:   c::bch_extent_ptr,
    new_blocknr: u32,
    stats:       UpdateStats,
}

/// Point an extent in a block of a reused stripe at the new one: its stripe
/// pointer dropped, its pointer moved to the new block and given the new
/// stripe's, then the replicas that's made extra dropped - the new pointer
/// kept.
fn stripe_update_extent(t: &TransAttempt<'_, '_>, u: &mut BlockUpdate<'_, '_>, bp: BkeySC<'_, c::Backpointer>,
                        ext: Extent<'_, '_>) -> Result<(), BchError> {
    let fs = t.fs();
    let (old_idx, new_idx) = (u.old.k.p.offset, u.new.k.p.offset);
    let (old_dev, new_dev) = (u32::from(u.old_block.dev()), u32::from(u.new_block.dev()));

    if bp.v().level != 0 {
        fs_inconsistent!(fs, "found btree node in erasure coded bucket:\n{}", bp.untyped().to_text(fs));
        return fs.throw(bch_errcode::BCH_ERR_erasure_coding_found_btree_node);
    }

    let Some((iter, k)) = ext else {
        // the extent's gone - flushing the write buffer and looking again
        // would say so for sure, but there's no need
        count(&mut u.stats.bp_to_deleted, bp);
        fs.event_inc_trace(events::stripe_update_extent_fail, |out| {
            prt!(out, "backpointer race\n{}\n{}", bp.untyped().to_text(fs), u.new.untyped().to_text(fs));
        });
        return Ok(());
    };

    let p = k.arm::<Ptrs>().and_then(|kp| kp.has_device_decode(fs, old_dev).map(|d| d.p))
        .filter(|p| ptr_in_block(&u.old_block, &p.ptr, u.old.v().sectors.get().into()));
    let Some(p) = p else {
        count(&mut u.stats.no_match, bp);
        fs.event_inc_trace(events::stripe_update_extent_fail, |out| {
            prt!(out, "block {}: nomatch\n{}\n{}\n{}", u.new_blocknr, bp.untyped().to_text(fs), k.to_text(fs),
                 u.new.untyped().to_text(fs));
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
            prt!(out, "block {}: cached pointer\n{}\n{}", u.new_blocknr, k.to_text(fs), u.new.untyped().to_text(fs));
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
        ptr.set_dev(u.new_block.dev());
        ptr.set_offset(ptr.offset() - u.old_block.offset() + u.new_block.offset());
        ptr.set_generation(u.new_block.generation());

        let at = kp.ptrs_decoded(fs).find(|d| u32::from(d.ptr.dev()) == new_dev).map(|d| d.pos)
            .expect("the pointer just moved");
        let stripe_ptr = c::bch_extent_stripe_ptr::new()
            .with_block(u.new_blocknr as u8)
            .with_redundancy(u.new.v().nr_redundant)
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

/// Every extent of block @old_blocknr of @old pointed at block @new_blocknr of
/// @new.
fn stripe_update_bucket(trans: &BtreeTrans<'_>, old: BkeySC<'_, c::Stripe<Validated>>,
                        new: BkeySC<'_, c::Stripe<Validated>>, old_blocknr: u32, new_blocknr: u32,
                        watermark: Watermark) -> Result<(), BchError> {
    let fs = trans.fs();
    let mut u = BlockUpdate {
        old, new,
        old_block:   old.v().block_ptrs()[old_blocknr as usize],
        new_block:   new.v().block_ptrs()[new_blocknr as usize],
        new_blocknr,
        stats:       UpdateStats::default(),
    };
    let res = DiskReservation::new(fs);

    block_for_each_extent(trans, old, old_blocknr, Some(&res),
                          CommitOpts::new().watermark(watermark).flags(CommitFlags::NO_CHECK_RW | CommitFlags::NO_ENOSPC),
                          |t, bp, ext| stripe_update_extent(t, &mut u, bp, ext))?;

    let s = &u.stats;
    fs.event_inc_trace(events::stripe_update_bucket, |out| {
        prt!(out, "Updating block {new_blocknr}\n{}\n", new.untyped().to_text(fs));
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
/// carried from @old - block old_block_map[i] there - the rest @new's own.
pub(crate) fn stripe_update_extents(trans: &BtreeTrans<'_>, old: BkeySC<'_, c::Stripe<Validated>>,
                                    new: BkeySC<'_, c::Stripe<Validated>>, old_block_map: &[u8], watermark: Watermark)
    -> Result<(), BchError>
{
    let nr_data = new.v().nr_data();
    let old_blocks_nr = old_block_map.len() as u32;

    trans.write_buffer_flush_sync()?;

    for i in 0..nr_data {
        if old_blocks_nr != 0 && i == old_blocks_nr {
            stripe_update_inject_crash(trans)?;
        }

        match old_block_map.get(i as usize) {
            Some(&old_blocknr) => stripe_update_bucket(trans, old, new, old_blocknr.into(), i, watermark)?,
            None               => stripe_update_bucket(trans, new, new, i, i, watermark)?,
        }
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
// What's here is deciding which. Reading the stripe and rebuilding it - the
// IO, and the allocator - are stripe_new.rs's: repair_read(), and
// repair_rebuild(), which takes over the stripe we have open and our read of
// it, and hands the new stripe to its task for its writes.
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
struct StripeBlocks {
    live: u32,
    bad:  u32,
}

impl StripeBlocks {
    fn get(fs: &Fs, s: BkeySC<'_, c::Stripe<Validated>>) -> Result<Self, BchError> {
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
/// stripe_new::repair_rebuild(), which takes over our @h and our read of the
/// stripe, @buf, and hands the new stripe to its task for its writes.
fn stripe_repair_rebuild(ctxt: &Moving<'_>, s: BkeySC<'_, c::Stripe<Validated>>, h: OpenStripe<'_>,
                         buf: StripeBuf, devs: c::bch_devs_mask, carry: u32, move_: u32)
    -> Result<StripeRepair, BchError>
{
    stripe_new::repair_rebuild(ctxt, s, h, buf, devs, carry, move_)?;
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
                               buf: StripeBuf, b: StripeBlocks) -> Result<StripeRepair, BchError>
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

    let (mut buf, read_all) = stripe_new::repair_read(trans, s, b.live)?;

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
