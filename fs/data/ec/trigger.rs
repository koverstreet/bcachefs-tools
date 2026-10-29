// SPDX-License-Identifier: GPL-2.0

//! The stripe key's trigger - ec/trigger.c's - and gc's copy of the stripes.
//!
//! A stripe update's trigger owns: each block's bucket - a parity block's
//! sectors, every block's stripe_refcount and bucket_to_stripe entry - and
//! its backpointer; parity replicas, reconcile work and fragmentation
//! accounting; the stripe lru, whose position (lru_pos()) the deleter and
//! stripe reuse walk; and deleting a stripe gone empty, unless it's open.
//! Its data blocks' sectors are their extents', counted from the extent
//! side, trigger_stripe_ptr() in alloc/buckets.rs - which updates the stripe
//! key, running this.

use crate::alloc::backpointers::{bucket_backpointer_mod, extent_ptr_to_bp};
use crate::alloc::buckets::bucket_to_u64;
use crate::alloc::lru::{lru_change, LruId};
use crate::alloc::replicas::{bkey_replicas_acct, bkey_replicas_padded, entry_acct};
use crate::btree::bkey::{pos, BkeyS, BkeySC, BkeyV, BkeyVMut};
use crate::btree::bkey_methods::TriggerOp;
use crate::btree::iter::{BtreeIter, BtreeIterFlags, TransAttempt, UpdateTriggerFlags};
use crate::c;
use crate::data::ec::stripe::{StripeK, STRIPE_LRU_POS_EMPTY};
use crate::data::ptrs::BkeyVPtrs;
use crate::data::reconcile::trigger::data_to_rb_work_pos;
use crate::errcode::{bch_errcode, BchError};
use crate::fs::Fs;
use crate::init::error::id;
use crate::init::passes;
use crate::util::alloc::AllocError;
use crate::util::ffi::Opaque;
use crate::util::kernel::{cold_path, unlikely};
use crate::util::log::LogMsg;
use crate::util::vstructs::plain_bytes;
use crate::{accounting, bch_err, fs_inconsistent, fsck_err_on, prt};
use arrayvec::ArrayVec;
use core::ops::ControlFlow;
use cstruct_macros::c_export;

// ── A bucket's stripes ───────────────────────────────────────────────────

impl TransAttempt<'_, '_> {
    /// How many stripes have a block in @bucket, by bucket_to_stripe.
    pub fn bucket_nr_stripes(&self, bucket: c::bpos) -> Result<u32, BchError> {
        let b = bucket_to_u64(bucket);
        let mut nr = 0;
        BtreeIter::new(self, c::btree_id::bucket_to_stripe, pos(b, 0), BtreeIterFlags::empty())
            .for_each_max_norestart(self, pos(b, u64::MAX), |_, _| {
                nr += 1;
                Ok(ControlFlow::<()>::Continue(()))
            })?;
        Ok(nr)
    }
}

// ── gc's stripes ─────────────────────────────────────────────────────────

impl c::bch_fs_ec {
    /// gc's copy of stripe @idx, if it has one.
    pub fn gc_stripe(&self, idx: u64) -> Option<&c::gc_stripe> {
        // SAFETY: an entry - a Lock, all of whose state is behind its lock
        self.gc_stripes.get_raw(idx as usize).map(|m| unsafe { m.as_ref() })
    }

    /// gc_stripe(), allocated - zeroed, not alive - if it wasn't.
    pub fn gc_stripe_alloc(&self, idx: u64, gfp: c::gfp_t) -> Result<&c::gc_stripe, AllocError> {
        self.gc_stripes.prealloc(idx as usize, gfp)?;
        Ok(self.gc_stripe(idx).expect("an entry, allocated"))
    }
}

#[c_export]
pub fn bch2_gc_stripe_get(c: &Opaque<c::bch_fs>, idx: u64) -> *mut c::gc_stripe {
    Fs::from_c(c).ec.gc_stripe(idx).map_or(core::ptr::null_mut(), |m| core::ptr::from_ref(m).cast_mut())
}

/// # Safety
/// gc is done with them: gc.pos is back to not_running. Not fenced from a
/// commit's gc trigger that saw it running - as reflink's gc table isn't:
/// see ReflinkGc.
#[c_export]
pub unsafe fn bch2_gc_stripes_free(c: &Opaque<c::bch_fs>) {
    unsafe { Fs::from_c(c).ec.gc_stripes.free() }
}

// ── The trigger ──────────────────────────────────────────────────────────

/// @k's stripe, if it's one - not a deletion.
fn stripe_of(k: BkeySC<'_>) -> Option<StripeK<'_>> {
    k.arm::<c::Stripe>()
}

/// Its time in the stripe lru - none for no stripe.
fn lru_pos(s: Option<StripeK<'_>>) -> u64 {
    s.map_or(0, |s| s.v().lru_pos())
}

fn needs_reconcile(s: Option<StripeK<'_>>) -> i64 {
    s.map_or(0, |s| s.v().needs_reconcile().into())
}

/// Whether going from @old to @new changes no block's bucket or backpointer.
fn blocks_unchanged(old: StripeK<'_>, new: StripeK<'_>) -> bool {
    let (o, n) = (old.v(), new.v());

    o.needs_reconcile() == n.needs_reconcile() &&
        o.nr_blocks == n.nr_blocks &&
        o.nr_redundant == n.nr_redundant &&
        plain_bytes(o.block_ptrs()) == plain_bytes(n.block_ptrs())
}

/// Refresh @new's can_widen from the RW members its disk label has now,
/// before lru_pos() reads it: else an update committing after reconcile's
/// stripes scan passed the stripe would keep a stale one, and the scan's
/// cookie be cleared with it wrong.
fn refresh_can_widen(fs: &Fs, new: &mut BkeyS<'_>) {
    let Some(mut s) = new.as_s().arm::<c::Stripe>() else { return };
    let v = s.v();
    let want = v.want_can_widen(fs.ec_rw_member_devs_nr(v.disk_label, v.sectors.get()),
                                fs.opts().ec_max_data_blocks.into());
    if v.can_widen() != want {
        s.v_mut().bits.set_can_widen(want);
    }
}

/// stripe_frag accounting - [data sectors, empty sectors], by how many of
/// its data blocks are empty - for @s, added (@sign 1) or taken away (-1):
/// two mods rather than one delta, as the position changes when the number
/// of empty blocks does.
fn stripe_frag_acct(t: &TransAttempt<'_, '_>, s: Option<StripeK<'_>>, gc: bool, sign: i64) -> Result<(), BchError> {
    let Some(s) = s else { return Ok(()) };
    let sectors = i64::from(s.v().sectors.get());
    let nr_data = s.data_blocks().count() as i64;
    let nr_empty = s.data_blocks().filter(|b| b.sectors() == 0).count();

    let acct = c::disk_accounting_pos::from_arm(c::bch_acct_stripe_frag { nr_empty: nr_empty as u8 });
    accounting::add(t, &acct, &[sign * nr_data * sectors, sign * nr_empty as i64 * sectors], gc)
}

/// dev_stripe_frag accounting - each device's [data sectors, empty sectors]
/// - from @old to @new: summed by device first, so an unchanged block nets to
/// zero, and the common case is one mod, not one per block.
fn dev_stripe_frag_acct(t: &TransAttempt<'_, '_>, old: Option<StripeK<'_>>, new: Option<StripeK<'_>>, gc: bool)
    -> Result<(), BchError>
{
    type Deltas = ArrayVec<(u8, i64, i64), { 2 * c::BCH_BKEY_PTRS_MAX as usize }>;

    fn add(d: &mut Deltas, s: Option<StripeK<'_>>, sign: i64) {
        let Some(s) = s else { return };
        let sectors = i64::from(s.v().sectors.get());

        for b in s.data_blocks() {
            let dev = b.ptr().dev();
            let i = match d.iter().position(|e| e.0 == dev) {
                Some(i) => i,
                None    => { d.push((dev, 0, 0)); d.len() - 1 }
            };
            d[i].1 += sign * sectors;
            d[i].2 += if b.sectors() == 0 { sign * sectors } else { 0 };
        }
    }

    let mut d = Deltas::new();
    add(&mut d, old, -1);
    add(&mut d, new, 1);

    for (dev, data, empty) in d {
        if data != 0 || empty != 0 {
            let acct = c::disk_accounting_pos::from_arm(c::bch_acct_dev_stripe_frag { dev });
            accounting::add(t, &acct, &[data, empty], gc)?;
        }
    }
    Ok(())
}

/// The alloc key and bucket_to_stripe disagree: we're adding a stripe
/// reference to a bucket whose refcount is already saturated, or dropping
/// one from a bucket that has none.
///
/// Saturate rather than wrap, because alloc_data_type() derives data_type
/// from the refcount - so wrapping changes what the bucket *is*, in both
/// directions:
///
///   0 - 1 makes an empty bucket BCH_DATA_stripe, which the alloc trigger
///   sees as a bucket going nonempty outside an open bucket, and takes the
///   filesystem read-only.
///
///   U32_MAX + 1 is quieter and worse. A stripe block holding no live
///   extents has no sectors of its own - the refcount is the whole
///   reservation - so zero reads as empty, and nothing stops the bucket
///   being discarded and handed out again while the stripe still points at
///   it.
///
/// check_alloc_info recounts stripe_refcount from bucket_to_stripe
/// (alloc_key_stripe_refcount_wrong, FSCK_AUTOFIX), so leave the true value
/// to it rather than guessing here.
#[cold]
fn stripe_refcount_saturated(fs: &Fs, bucket: c::bpos, stripe: u64, overflow: bool) {
    let mut msg = LogMsg::new(fs);
    prt!(&mut msg, "stripe_refcount {} at bucket {}:{} {} stripe {stripe}\n",
         if overflow { "overflow" } else { "underflow" }, { bucket.inode }, { bucket.offset },
         if overflow { "adding ref to" } else { "dropping ref to" });

    let _ = passes::run_explicit(fs, &mut msg, c::bch_recovery_pass::BCH_RECOVERY_PASS_check_alloc_info,
                                 c::bch_run_recovery_pass_flags::from_bits_retain(0));
}

/// Block @i's part of @a, the alloc key of its bucket @bucket, for @s going
/// in or (@deleting) out: __mark_stripe_bucket(). A parity block's sectors
/// are the stripe's; a data block's are its extents', which their triggers
/// count.
#[allow(clippy::too_many_arguments)]
fn mark_stripe_bucket_alloc(t: &TransAttempt<'_, '_>, ca: &c::bch_dev, s: StripeK<'_>, i: u32, deleting: bool,
                            bucket: c::bpos, a: &mut c::AllocV4, flags: UpdateTriggerFlags) -> Result<(), BchError> {
    use c::bch_data_type as D;
    let fs = t.fs();
    let v = s.v();
    let ptr = &s.v().block_ptrs()[i as usize];
    let parity = i >= v.nr_data();
    let data_type = if parity { D::BCH_DATA_parity } else { D::BCH_DATA_stripe };

    let sectors = if parity { i64::from(v.sectors.get()) } else { 0 };
    if sectors != 0 {
        let sectors = if deleting { -sectors } else { sectors };
        let (generation, mut bucket_data_type) = (a.generation, a.data_type);
        let ret = t.bucket_ref_update(ca, s.untyped(), ptr, sectors, data_type, generation, &mut bucket_data_type,
                                      &mut a.dirty_sectors);
        // as C's, even on error
        a.data_type = bucket_data_type;
        ret?;
    }

    if flags.contains(UpdateTriggerFlags::TRANSACTIONAL) {
        t.bit_mod(c::btree_id::bucket_to_stripe, pos(bucket_to_u64(bucket), s.k.p.offset), !deleting)?;
    }

    if !deleting {
        match a.stripe_refcount.checked_add(1) {
            Some(n) => a.stripe_refcount = n,
            None    => stripe_refcount_saturated(fs, bucket, s.k.p.offset, true),
        }
    } else {
        match a.stripe_refcount.checked_sub(1) {
            Some(n) => a.stripe_refcount = n,
            None    => stripe_refcount_saturated(fs, bucket, s.k.p.offset, false),
        }
    }

    let inconsistent = |a: &c::AllocV4| (a.stripe_refcount == 0) != (a.dirty_sectors == 0);
    if parity && inconsistent(a) {
        // XXX: with gc this runs under the gc bucket's lock - gc_bucket_mod()
        // - and a btree walk and an fsck error can sleep. As C's.
        let nr_stripes = t.bucket_nr_stripes(bucket)?;

        if fsck_err_on!(t, a.stripe_refcount != nr_stripes, id::alloc_key_stripe_refcount_wrong,
                        "bucket {}:{} with incorrect stripe_refcount, should be {nr_stripes}",
                        { bucket.inode }, { bucket.offset })? {
            a.stripe_refcount = nr_stripes;
        }

        if inconsistent(a) {
            let mut msg = LogMsg::new(fs);
            prt!(&mut msg, "Parity bucket with dirty_sectors/stripe_refcount inconsistency at {}:{}\n",
                 { bucket.inode }, { bucket.offset });
            prt!(&mut msg, "stripe_refcount {} but dirty_sectors {}\n", a.stripe_refcount, a.dirty_sectors);

            let flags = c::bch_run_recovery_pass_flags::from_bits_retain(0);
            let _ = passes::run_explicit(fs, &mut msg, c::bch_recovery_pass::BCH_RECOVERY_PASS_check_allocations, flags);
            let _ = passes::run_explicit(fs, &mut msg, c::bch_recovery_pass::BCH_RECOVERY_PASS_check_alloc_info, flags);

            // so the bucket isn't reused until it's repaired
            a.stripe_refcount = a.stripe_refcount.max(1);
            a.dirty_sectors   = a.dirty_sectors.max(1);
        }
    }

    a.set_data_type(if a.stripe_refcount != 0 { data_type } else { D::BCH_DATA_user });
    Ok(())
}

/// Block @i's bucket, for @s going in or (@deleting) out: its alloc key, and
/// gc's copy with gc.
fn mark_stripe_bucket(t: &TransAttempt<'_, '_>, s: StripeK<'_>, i: u32, deleting: bool, flags: UpdateTriggerFlags)
    -> Result<(), BchError>
{
    let fs = t.fs();
    let ptr = &s.v().block_ptrs()[i as usize];
    let dev = u32::from(ptr.dev());

    // C's bch2_dev_bkey_tryget() drops the error of scheduling the repair
    // of a missing device it reports
    let Some(ca) = fs.dev_bkey_tryget(s.untyped(), dev).unwrap_or(None) else {
        cold_path();
        if dev != c::BCH_SB_MEMBER_INVALID && !flags.contains(UpdateTriggerFlags::OVERWRITE) {
            return fs.throw(bch_errcode::BCH_ERR_mark_stripe);
        }
        return Ok(());
    };
    let bucket = ca.ptr_bucket_pos(ptr);

    if flags.contains(UpdateTriggerFlags::TRANSACTIONAL) {
        let mut a = t.start_alloc_update(bucket, UpdateTriggerFlags::empty())?;
        mark_stripe_bucket_alloc(t, &ca, s, i, deleting, bucket, a.v_mut(), flags)?;
    }

    if flags.contains(UpdateTriggerFlags::GC) {
        let Some(g) = ca.gc_bucket(bucket.offset) else {
            fs_inconsistent!(fs, "reference to invalid bucket on device {dev}\n{}", s.to_text(fs));
            return fs.throw(bch_errcode::BCH_ERR_mark_stripe);
        };
        t.gc_bucket_mod(&ca, g, flags, |a| mark_stripe_bucket_alloc(t, &ca, s, i, deleting, bucket, a, flags))?;
    }
    Ok(())
}

/// Block @i's backpointer, inserted or deleted - none for a block on no
/// device.
fn mark_stripe_bp(t: &TransAttempt<'_, '_>, s: StripeK<'_>, i: u32, insert: bool) -> Result<(), BchError> {
    let fs = t.fs();
    let d = s.ptrs_decoded(fs).nth(i as usize).expect("a block of the stripe's");
    if u32::from(d.p.ptr.dev()) == c::BCH_SB_MEMBER_INVALID {
        return Ok(());
    }

    let mut bp = extent_ptr_to_bp(fs, c::btree_id::stripes, 0, s.untyped(), &d);
    bucket_backpointer_mod(t, s.untyped(), &mut bp, insert)
}

/// Each block whose pointer changed: its bucket - the new one's in, the old
/// one's out - and its backpointer, which a change in needs_reconcile moves
/// too (the backpointer carries the reconcile phys bits).
fn mark_stripe_buckets(t: &TransAttempt<'_, '_>, old: Option<StripeK<'_>>, new: Option<StripeK<'_>>,
                       flags: UpdateTriggerFlags) -> Result<(), BchError> {
    fn block<'a>(s: Option<StripeK<'a>>, i: u32) -> Option<&'a c::bch_extent_ptr> {
        s.and_then(|s| s.v().block_ptrs().get(i as usize))
    }
    let nr_blocks = |s: Option<StripeK<'_>>| s.map_or(0, |s| u32::from(s.v().nr_blocks));
    let reconcile_changed = needs_reconcile(old) != needs_reconcile(new);

    for i in 0..nr_blocks(old).max(nr_blocks(new)) {
        let (old_ptr, new_ptr) = (block(old, i), block(new, i));
        let changing = match (old_ptr, new_ptr) {
            (Some(o), Some(n)) => plain_bytes(o) != plain_bytes(n),
            _                  => true,
        };

        if changing {
            if let (Some(n), Some(_)) = (new, new_ptr) {
                mark_stripe_bucket(t, n, i, false, flags)?;
            }
            if let (Some(o), Some(_)) = (old, old_ptr) {
                mark_stripe_bucket(t, o, i, true, flags)?;
            }
        }

        if (changing || reconcile_changed) && flags.contains(UpdateTriggerFlags::TRANSACTIONAL) {
            if let (Some(o), Some(_)) = (old, old_ptr) {
                mark_stripe_bp(t, o, i, false)?;
            }
            if let (Some(n), Some(_)) = (new, new_ptr) {
                mark_stripe_bp(t, n, i, true)?;
            }
        }
    }
    Ok(())
}

/// The stripe key's trigger: bch2_trigger_stripe().
///
/// Transactional: the new key's can_widen refreshed; an empty stripe that
/// isn't open deleted; its place in the stripe lru, and its reconcile_hipri
/// bit. With gc as well: reconcile work, fragmentation and parity replicas
/// accounting - gc's copy of the stripe made - and each changed block's
/// bucket and backpointer. Atomic: a stripe gone empty kicks the deleter.
fn trigger(t: &TransAttempt<'_, '_>, op: &mut TriggerOp<'_>) -> Result<(), BchError> {
    use UpdateTriggerFlags as F;
    let fs = t.fs();
    let flags = op.flags;
    let gc = flags.contains(F::GC);
    let idx = op.new.k.p.offset;

    // before anything reads the new key: its csum check, below, doesn't
    // look at can_widen
    if flags.contains(F::TRANSACTIONAL) {
        refresh_can_widen(fs, &mut op.new);
    }

    let old = stripe_of(op.old);
    let new = stripe_of(BkeySC::from(&op.new));

    if let Some(s) = old {
        s.v().csum_type_check(fs)?;
    }
    if let Some(s) = new {
        s.v().csum_type_check(fs)?;
    }
    if let (Some(o), Some(n)) = (old, new) {
        let (o, n) = (o.v(), n.v());
        assert!(n.sectors.get() == o.sectors.get() && n.nr_blocks == o.nr_blocks && n.nr_redundant == o.nr_redundant,
                "a stripe update changing its shape");
    }

    let needs_reconcile_delta = needs_reconcile(new) - needs_reconcile(old);

    if flags & (F::ATOMIC | F::GC) == F::ATOMIC && new.is_some() && lru_pos(new) == STRIPE_LRU_POS_EMPTY {
        fs.do_stripe_deletes();
    }

    if flags.contains(F::TRANSACTIONAL) {
        let (old_lru, new_lru) = (lru_pos(old), lru_pos(new));

        // stripes is key-cached: deleted by an update, which goes through
        // the key cache's flush - editing @op.new into a deletion would
        // bypass it, and trip the commit's assert. The btree's delete runs
        // this trigger again, which cleans up after it.
        if unlikely(new_lru == STRIPE_LRU_POS_EMPTY) && !fs.stripe_is_open(idx) {
            return t.delete(c::btree_id::stripes, op.new.k.p, F::OVERWRITE_TRIGGERED);
        }

        lru_change(t, LruId::Stripes, idx, old_lru, new_lru)?;

        if needs_reconcile_delta != 0 {
            t.bit_mod_buffered(c::btree_id::reconcile_hipri, data_to_rb_work_pos(c::btree_id::stripes, op.new.k.p),
                               needs_reconcile_delta > 0)?;
        }
    }

    if !flags.intersects(F::TRANSACTIONAL | F::GC) {
        return Ok(());
    }

    if needs_reconcile_delta != 0 {
        let s = old.map(|s| s.v()).or(new.map(|s| s.v())).expect("a stripe, if needs_reconcile changed");
        let acct = c::disk_accounting_pos::from_arm(c::bch_acct_reconcile_work {
            type_: c::bch_reconcile_accounting_type::BCH_RECONCILE_ACCOUNTING_stripes.0 as u8,
        });
        let sectors = i64::from(s.nr_blocks) * i64::from(s.sectors.get()) * needs_reconcile_delta;
        accounting::add(t, &acct, &[sectors, 0], gc)?;
    }

    // above the unchanged return: block counts change without the pointers
    // changing, and they're what this counts
    stripe_frag_acct(t, old, gc, -1)?;
    stripe_frag_acct(t, new, gc, 1)?;
    dev_stripe_frag_acct(t, old, new, gc)?;

    if old.zip(new).is_some_and(|(o, n)| blocks_unchanged(o, n)) {
        return Ok(());
    }

    // XXX: wrong when runtime gc comes back, which should unmark the old key
    // and mark the new - with locking
    let m = if gc {
        let new = new.expect("gc marks keys going in");
        let Ok(m) = fs.ec.gc_stripe_alloc(idx, c::GFP_KERNEL) else {
            bch_err!(fs, "error allocating memory for gc_stripes, idx {idx}");
            return fs.throw(bch_errcode::BCH_ERR_ENOMEM_mark_stripe);
        };

        let v = new.v();
        let mut d = m.lock.lock();
        d.set_alive(1);
        d.sectors      = v.sectors.get();
        d.nr_blocks    = v.nr_blocks;
        d.nr_redundant = v.nr_redundant;
        for (i, p) in v.block_ptrs().iter().enumerate() {
            d.ptrs[i] = *p;
        }
        // gc counts them from the stripe pointers
        d.block_sectors = Default::default();
        Some(m)
    } else {
        None
    };

    let parity = c::bch_data_type::BCH_DATA_parity;
    if let Some(n) = new {
        let r = bkey_replicas_padded(fs, n.untyped());
        let sectors = i64::from(n.v().sectors.get()) * i64::from(n.v().nr_redundant);
        // SAFETY: any bytes are an entry's
        accounting::add(t, &entry_acct(unsafe { &r.bytes }, parity), &[sectors], gc)?;
        if let Some(m) = m {
            m.lock.lock().r = r;
        }
    }
    if let Some(o) = old {
        let sectors = -(i64::from(o.v().sectors.get()) * i64::from(o.v().nr_redundant));
        accounting::add(t, &bkey_replicas_acct(fs, o.untyped(), parity), &[sectors], gc)?;
    }

    mark_stripe_buckets(t, old, new, flags)
}

crate::key_trigger!(bch2_trigger_stripe => trigger);

// ── gc's copy, before the commit ─────────────────────────────────────────

/// While gc's running, its copy of the stripe at @iter, allocated - the
/// transaction's locks dropped, if they have to be - before the commit its
/// trigger fills it in.
pub fn ec_stripe_mem_alloc(t: &TransAttempt<'_, '_>, iter: &BtreeIter<'_>) -> Result<(), BchError> {
    let fs = t.fs();
    let idx = iter.pos().offset;

    t.allocate_dropping_locks(|gfp| {
        if fs.gc.phase() != c::gc_phase::GC_PHASE_not_running && fs.ec.gc_stripe_alloc(idx, gfp).is_err() {
            return fs.throw(bch_errcode::BCH_ERR_ENOMEM_ec_stripe_mem_alloc);
        }
        Ok(())
    })
}
