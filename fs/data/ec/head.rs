// SPDX-License-Identifier: GPL-2.0

//! Stripe heads - a disk label's, redundancy's and watermark's stripe
//! being made - and the devices a stripe can be made on: which a head
//! allocates from, and a disk label's striping state across them.
//!
//! A stripe's devices are its disk label's, 0 for every device's: two views
//! of them. disk_label_ec_devs() is what a stripe can be made on now - rw
//! for user data, durable, with one bucket size, the most common - which is
//! what stripe heads allocate from and what repair has to work with.
//! disk_label_ec_rw_member_devs() is the configured view, online or not,
//! that a stripe's can_widen is measured against: so a device going offline
//! and coming back doesn't have reconcile rewrite every stripe's.

use crate::alloc::disk_groups::Target;
use crate::btree::bkey::BkeyV;
use crate::btree::iter::{BtreeTrans, TransAttempt};
use crate::c;
use crate::data::ec::buf::{MemWait, StripeBuf};
use crate::data::ec::stripe_new::{self, Internal, Slot, StripeBlock, StripeNew, StripeNewBox};
use crate::data::ec::stripe_new_reuse::OldRead;
use crate::data::ec::stripe_new_blocks::stripe_alloc_or_reuse;
use crate::errcode::{bch_errcode, err_ptr, BchError};
use crate::fs::Fs;
use crate::locking::Guard;
use crate::sb::counters::events;
use crate::util::alloc::flags::GFP_KERNEL;
use crate::util::alloc::kbox_pin_init;
use crate::util::ffi::Opaque;
use crate::util::rcu::{read_lock, ReadGuard};
use crate::util::Printbuf;
use crate::{bch_err, new_lock, prt};
use core::ffi::{c_int, c_uint, c_ulong};
use core::ptr::NonNull;
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

// ── Stripe heads ─────────────────────────────────────────────────────────
//
// The sector allocator gets erasure coded buckets from a stripe head - one
// per disk label, algorithm, redundancy and watermark - which stages the
// stripe being filled. The head is locked from lookup until
// bch2_ec_stripe_head_put(): bch2_ec_stripe_head_get(), below,
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
        s.err.set(err);
    }
    set_pending(fs, s);
}

/// @h's devices, as of the allocator's RW device changes @change_count: a
/// stripe not yet allocated that some have left is cancelled.
fn stripe_head_devs_update(fs: &Fs, h: &c::ec_stripe_head, g: &mut HeadGuard<'_>, change_count: c_ulong) {
    let (mask, blocksize) = fs.disk_label_ec_devs(h.disk_label, 0);
    let mut leaving = mask;
    leaving.d.not_and(&g.devs.mask.d);

    g.devs.change_count = change_count;
    g.devs.nr_active    = mask.d.weight();
    g.devs.insufficient = !fs.ec_devs_sufficient(&mask, h.disk_label, h.redundancy, None);
    g.devs.blocksize    = blocksize;
    g.devs.mask         = mask;

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
    if g.devs.change_count != count {
        stripe_head_devs_update(fs, h, &mut g, count);
    }

    if g.devs.insufficient {
        return fs.throw(bch_errcode::BCH_ERR_ec_alloc_failed_insufficient_devs);
    }
    Ok((h, g))
}

/// Unlock the head @g locks, handing on its stripe if every data block's been
/// claimed: nothing more can go into it.
pub(crate) fn stripe_head_put(fs: &Fs, mut g: HeadGuard<'_>) {
    let full = head_stripe(&g).is_some_and(|s| {
        let s = s.lock();
        s.allocated && s.blocks[..s.nr_data() as usize].iter().all(Slot::allocated)
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
        let ob_dev = s.blocks[i].bucket().map(|b| b.dev());
        // a reused block has no bucket of ours: init_new_stripe_from_old()
        // copies the old pointer into the key
        ob_dev == Some(ca.dev_idx.into()) ||
            (s.blocks[i].allocated() && v.block_ptrs()[i].dev() == ca.dev_idx)
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
            assert!(s.blocks[..nr_blocks].iter().all(|b| b.bucket().is_none()),
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

// ── The head's: staging, and claiming blocks ──────────────────────────────

/// A new stripe for @h: as wide as @h's devices allow - one block per
/// failure domain is a hard requirement, so the width is capped by domains
/// as well as devices; if a domain goes away the next stripe is narrower
/// rather than doubling up.
fn alloc_for_head(fs: &Fs, h: &c::ec_stripe_head, g: &mut HeadGuard<'_>, max_data_blocks: u32)
    -> Result<StripeNewBox, BchError>
{
    let active = g.devs.nr_active.min(c::BCH_BKEY_PTRS_MAX);
    let nr_data = (active - h.redundancy).min(if max_data_blocks != 0 { max_data_blocks } else { !0 });

    // insufficiency was checked - at least redundancy + 2 domains:
    let nr_domains = fs.target_nr_domains(&g.devs.mask);
    let nr_data = nr_data.min(nr_domains - h.redundancy);

    let failed = || {
        bch_err!(fs, "failed to allocate new stripe");
        fs.err(bch_errcode::BCH_ERR_ENOMEM_ec_new_stripe_alloc)
    };
    // its room in new_stripes, for when it's handed on
    fs.ec.new_stripes.lock().reserve_for_head().map_err(|_| failed())?;
    let s = StripeNew::alloc(fs, g.devs.mask, h.watermark, h.disk_label, h.algo, nr_data, h.redundancy, g.devs.blocksize)
        .map_err(|_| {
            fs.ec.new_stripes.lock().release_for_head();
            failed()
        })?;
    g.nr_created += 1;
    Ok(s)
}

/// Hand @b on to its task, as it is: it's no longer its head's, and goes in
/// new_stripes, its own io ref put. The caller's lock - the head's - is held.
pub(crate) fn set_pending(fs: &Fs, b: StripeNewBox) {
    {
        let i = b.lock();
        assert!(i.allocated || b.err.get().is_some(),
                "stripe {} handed on, neither allocated nor cancelled", i.new_stripe.idx());
    }

    let mut v = fs.ec.new_stripes.lock();
    v.push_from_head(b);
    v.last().expect("just pushed").fill(&v);
}

/// A block of @s, @i its internal state, for this write: the first one on a
/// device we may allocate from that nobody else has taken - its io ref, the
/// writer's. NULL means this stripe has nothing for us: its free blocks are
/// all on devices we're excluded from.
fn claim_bucket(req: &c::alloc_request, s: &StripeNew, i: &mut Internal) -> *mut c::open_bucket {
    let nr_data = i.nr_data() as usize;
    for &dev in &req.devs_sorted.data[..req.devs_sorted.nr as usize] {
        for ec_idx in 0..nr_data {
            let Slot::Bucket(StripeBlock::Pinned(b)) = &i.blocks[ec_idx] else { continue };
            if b.dev() == u32::from(dev) {
                let Slot::Bucket(StripeBlock::Pinned(b)) = core::mem::take(&mut i.blocks[ec_idx]) else {
                    unreachable!()
                };
                let (claimed, ob) = b.claim(s.as_c(), ec_idx as u8);
                i.blocks[ec_idx] = Slot::Bucket(StripeBlock::Claimed(claimed));
                s.io_get();
                return ob;
            }
        }
    }
    core::ptr::null_mut()
}

/// @h's stripe staged - allocated, its buffers, its task started - and a
/// block of it claimed for this write, if @want_ob: NULL if it has none for
/// us.
fn head_stage(t: &TransAttempt<'_, '_>, req: &mut c::alloc_request, h: &c::ec_stripe_head, g: &mut HeadGuard<'_>,
              want_ob: bool) -> Result<*mut c::open_bucket, BchError> {
    let fs = t.fs();
    if g.s.is_none() {
        let s = alloc_for_head(fs, h, g, req.ec_max_data_blocks.into())?;
        g.s = Some(s);
    }

    let s = g.s.as_deref().expect("the head's stripe, just made");
    let mut i = s.lock();
    if !i.allocated {
        stripe_alloc_or_reuse(t, req, dev_stripe_state_get(t, h.disk_label)?, &mut i, s.fs_ref())?;

        fs.event_inc_trace(events::stripe_alloc, |out| {
            prt!(out, "\nnew: {}", i.new_stripe.stripe().to_text(fs));
            if let Some(r) = &i.reuse {
                prt!(out, "\nold: {}", r.stripe.stripe().to_text(fs));
            }
        });
    }

    if i.new_buf.is_none() {
        let wait = NonNull::new(req.cl).map_or(MemWait::Never, MemWait::Closure);
        i.new_buf = Some(StripeBuf::new_whole(fs, i.new_stripe.stripe(), wait)?);
    }

    if let Some(r) = i.reuse.as_mut().filter(|r| r.read == OldRead::NoBuf) {
        r.buf = Some(StripeBuf::new_whole(fs, r.stripe.stripe(), MemWait::Never)?);
        r.read = OldRead::ToRead;
    }

    s.launch(&mut i);

    // Has to come after the stripe's own allocation: that runs the bucket
    // allocator against the stripe's device mask, clobbering devs_sorted.
    let ob = if want_ob {
        // SAFETY: the write's write point, the caller's
        fs.dev_alloc_list(unsafe { &mut (*req.wp).alloc.stripe }, req);
        claim_bucket(req, s, &mut i)
    } else {
        core::ptr::null_mut()
    };

    t.verify_not_restarted();
    Ok(ob)
}

/// The stripe head for @req's erasure coding, locked, with a block of its
/// stripe claimed for this write in @ob - if @ob isn't NULL, and it had one
/// for us: or the error, an ec_alloc_failed subtype naming the reason.
/// bch2_ec_stripe_head_get().
fn head_get<'f>(t: &TransAttempt<'_, 'f>, req: &mut c::alloc_request, algo: u32, want_ob: bool)
    -> Result<(&'f c::ec_stripe_head, HeadGuard<'f>, *mut c::open_bucket), BchError>
{
    let fs = t.fs();
    let redundancy = u32::from(req.ec_replicas).wrapping_sub(1);
    let disk_label = match Target::decode(req.target) {
        Target::Group(g) if g > u8::MAX.into() => return fs.throw(bch_errcode::BCH_ERR_ec_alloc_failed_disk_label_too_big),
        Target::Group(g) => g + 1, /* 0 == no label */
        _ => 0,
    };

    let (h, mut g) = stripe_head_get(t, disk_label, algo, redundancy, req.watermark)?;
    match head_stage(t, req, h, &mut g, want_ob) {
        Ok(ob) => Ok((h, g, ob)),
        Err(e) => {
            stripe_head_put(fs, g);
            // Not fatal to the write - the allocator falls back to replication:
            Err(if e.matches(bch_errcode::BCH_ERR_stripe_insufficient_devices) {
                fs.err(bch_errcode::BCH_ERR_ec_alloc_failed_stripe_alloc)
            } else {
                e
            })
        }
    }
}

/// head_get(), for C: the head locked, until bch2_ec_stripe_head_put().
///
/// # Safety
/// @trans in an attempt; @req the caller's, its write point too; @ob NULL,
/// or where to put the block claimed.
#[c_export]
pub unsafe fn bch2_ec_stripe_head_get<'t>(trans: &'t BtreeTrans<'t>, req: &mut c::alloc_request, algo: c_uint,
                                          ob: *mut *mut c::open_bucket) -> *mut c::ec_stripe_head {
    // SAFETY: the caller's
    let t = unsafe { trans.attempt_in_progress() };
    if let Some(ob) = unsafe { ob.as_mut() } {
        *ob = core::ptr::null_mut();
    }
    match head_get(&t, req, algo, !ob.is_null()) {
        Ok((h, g, got)) => {
            g.leave_locked();
            if let Some(ob) = unsafe { ob.as_mut() } {
                *ob = got;
            }
            core::ptr::from_ref(h).cast_mut()
        }
        Err(e) => err_ptr(e),
    }
}
