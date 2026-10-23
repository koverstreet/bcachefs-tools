// SPDX-License-Identifier: GPL-2.0

use crate::accounting::{self, bucket_data_type_mismatch, data_type, data_type_from_u8, mod_dev_cached_sectors};
use crate::alloc::backpointers::{bucket_backpointer_mod, extent_ptr_to_bp};
use crate::alloc::replicas::{bkey_replicas_acct, entry_acct, replicas_acct};
use crate::btree::bkey::prelude::*;
use crate::btree::bkey::{pos, BkeySC};
use crate::btree::bkey_methods::TriggerOp;
use crate::btree::iter::{commit_do, BtreeIter, BtreeIterFlags, BtreeTrans, CommitOpts, TransAttempt,
                         UpdateTriggerFlags, Watermark};
use crate::c;
use crate::c::bch_errcode;
use crate::data::ec::trigger::{ptr_matches_stripe, ptr_matches_stripe_m, stripe_blockcount_add};
use crate::data::ptrs::{BkeyVPtrs, DecodedPtr, Ptrs};
use crate::data::reconcile::needs_reconcile::extent_trigger_set_needs_reconcile;
use crate::data::reconcile::trigger::trigger_extent_reconcile;
use crate::errcode::{ret_to_c, ret_to_result_void as ret_to_result, BchError};
use crate::fs::Fs;
use crate::init::error::{count_fsck_err, fs_inconsistent, id};
use crate::init::passes;
use crate::opts::{data_type_str, prt_data_type};
use crate::locking::Lock;
use crate::types::{bits_to_longs, Le64};
use crate::util::ffi::Opaque;
use crate::util::log::LogMsg;
use crate::util::percpu::{Counters, PerCpu};
use crate::util::rcu::{read_lock, ReadGuard};
use crate::util::{Printbuf, ToText};
use crate::{bch_err, bch_err_fn, btree_trans, fsck_err_on, log_msg_ratelimited, message, percpu_field, prt};
use core::cell::UnsafeCell;
use core::ffi::{c_int, c_uint, c_ulong};
use core::mem::size_of;
use core::sync::atomic::Ordering;
use cstruct_macros::c_export;

// ── Bucket addressing ────────────────────────────────────────────────────

/// A device's sectors as its buckets: alloc/buckets.h's inlines.
impl c::bch_dev {
    /// sector_to_bucket().
    pub fn sector_to_bucket(&self, s: u64) -> u64 {
        s / u64::from(self.mi.bucket_size)
    }

    /// bucket_to_sector().
    pub fn bucket_to_sector(&self, b: u64) -> u64 {
        b * u64::from(self.mi.bucket_size)
    }

    /// The bucket sector @s is in, and how far into it:
    /// sector_to_bucket_and_offset().
    pub fn sector_to_bucket_and_offset(&self, s: u64) -> (u64, u32) {
        let size = u64::from(self.mi.bucket_size);
        (s / size, (s % size) as u32)
    }

    /// Its bucket size, in bytes.
    pub fn bucket_bytes(&self) -> u64 {
        self.bucket_to_sector(1) << 9
    }

    /// The bucket @ptr points into: PTR_BUCKET_NR().
    pub fn ptr_bucket(&self, ptr: &c::bch_extent_ptr) -> u64 {
        self.sector_to_bucket(ptr.offset())
    }

    /// The bucket @ptr points into, as an alloc btree position:
    /// PTR_BUCKET_POS().
    pub fn ptr_bucket_pos(&self, ptr: &c::bch_extent_ptr) -> c::bpos {
        pos(ptr.dev().into(), self.ptr_bucket(ptr))
    }
}

// ── GC buckets ───────────────────────────────────────────────────────────

impl c::bch_dev {
    /// Bucket @b's gc state, if it's one of the device's: gc_bucket().
    pub fn gc_bucket(&self, b: u64) -> Option<&c::bucket> {
        self.bucket_valid(b).then(|| self.buckets_gc.get(b as usize)).flatten()
    }
}

impl TransAttempt<'_, '_> {
    /// Gc bucket @g's state, as an alloc key, edited by @f under its lock -
    /// left as it was if @f fails - and device @ca's counters moved from the
    /// old to the new after: a trigger's gc side, on a bucket.
    pub fn gc_bucket_mod(&self, ca: &c::bch_dev, g: &c::bucket, flags: UpdateTriggerFlags,
                         f: impl FnOnce(&mut c::AllocV4) -> Result<(), BchError>) -> Result<(), BchError> {
        let (old, new) = {
            let mut b = g.lock.lock();
            let old = b.alloc();
            let mut new = old;
            f(&mut new)?;
            b.set_alloc(&new);
            (old, new)
        };
        self.alloc_key_to_dev_counters(ca, &old, &new, flags)
    }
}

/// The fields a gc bucket shares with its alloc key.
impl c::BucketData {
    /// Into @a, the rest of @a as it was: __bucket_m_to_alloc().
    pub fn to_alloc(self, a: &mut c::AllocV4) {
        a.generation     = self.generation;
        a.data_type      = self.data_type();
        a.stripe_sectors = self.stripe_sectors;
        a.dirty_sectors  = self.dirty_sectors;
        a.cached_sectors = self.cached_sectors;
    }

    /// As an alloc key, the rest zeroed: bucket_m_to_alloc().
    pub fn alloc(self) -> c::AllocV4 {
        let mut a = c::AllocV4::default();
        self.to_alloc(&mut a);
        a
    }

    /// From @a: alloc_to_bucket().
    pub fn set_alloc(&mut self, a: &c::AllocV4) {
        self.generation     = a.generation;
        self.set_data_type(a.data_type);
        self.stripe_sectors = a.stripe_sectors;
        self.dirty_sectors  = a.dirty_sectors;
        self.cached_sectors = a.cached_sectors;
    }
}

// ── Bucket references ────────────────────────────────────────────────────

impl TransAttempt<'_, '_> {
    /// Add @sectors - negative for a pointer going away - to @bucket_sectors,
    /// one of the sector counts of the bucket @ptr points into, checking the
    /// pointer against the bucket: its gen @b_gen, its @bucket_data_type. A
    /// pointer that doesn't fit is fsck's to repair: an error on insert, and
    /// ignored on overwrite, so deletion works; a stale cached pointer is
    /// BCH_ERR_cached_ptr_stale: bch2_bucket_ref_update().
    #[allow(clippy::too_many_arguments)]
    pub fn bucket_ref_update(&self, ca: &c::bch_dev, k: BkeySC<'_>, ptr: &c::bch_extent_ptr,
                             sectors: i64, ptr_data_type: c::bch_data_type, b_gen: u8,
                             bucket_data_type: &mut u8, bucket_sectors: &mut u32) -> Result<(), BchError> {
        assert!(sectors != 0);

        // as C's: a negative @sectors past the count wraps, and is past U32_MAX
        let new = u64::from(*bucket_sectors).wrapping_add(sectors as u64);
        if b_gen != ptr.generation() ||
           bucket_data_type_mismatch(data_type_from_u8(*bucket_data_type), ptr_data_type) ||
           new > u64::from(u32::MAX) {
            return self.bucket_ref_update_slowpath(ca, k, ptr, sectors, ptr_data_type, b_gen,
                                                   bucket_data_type, bucket_sectors);
        }

        *bucket_sectors = new as u32;
        Ok(())
    }

    #[cold]
    #[inline(never)]
    #[allow(clippy::too_many_arguments)]
    fn bucket_ref_update_slowpath(&self, ca: &c::bch_dev, k: BkeySC<'_>, ptr: &c::bch_extent_ptr,
                                  sectors: i64, ptr_data_type: c::bch_data_type, b_gen: u8,
                                  bucket_data_type: &mut u8, bucket_sectors: &mut u32) -> Result<(), BchError> {
        let fs = self.fs();
        let (dev, b, ptr_gen) = (ptr.dev(), ca.ptr_bucket(ptr), ptr.generation());
        let inserting = sectors > 0;
        // the bucket's type, or the pointer's if it hasn't one
        let type_str = |t: u8| data_type_str(if t != 0 { data_type_from_u8(t) } else { ptr_data_type });

        assert!(sectors != 0);

        if gen_after(ptr_gen, b_gen) > 0 {
            return self.bucket_ref_update_err(k, inserting, id::ptr_gen_newer_than_bucket_gen, message!(
                "bucket {dev}:{b} gen {b_gen} data type {}: ptr gen {ptr_gen} newer than bucket gen",
                type_str(*bucket_data_type)));
        }

        if i32::from(gen_cmp(b_gen, ptr_gen)) > c::BUCKET_GC_GEN_MAX as i32 {
            return self.bucket_ref_update_err(k, inserting, id::ptr_too_stale, message!(
                "bucket {dev}:{b} gen {b_gen} data type {}: ptr gen {ptr_gen} too stale",
                type_str(*bucket_data_type)));
        }

        if b_gen != ptr_gen && ptr.cached() != 0 {
            let no_stale_ptrs = 1 << c::bch_sb_compat::BCH_COMPAT_no_stale_ptrs.0;
            if fsck_err_on!(self, fs.sb.compat.get() & no_stale_ptrs != 0,
                            id::stale_ptr_with_no_stale_ptrs_feature,
                            "stale cached ptr, but have no_stale_ptrs feature\n{}", k.to_text(fs))? {
                let mut sb = fs.sb_lock();
                let compat = &mut sb.sb_mut().compat[0];
                *compat = Le64::new(compat.get() & !no_stale_ptrs);
                fs.write_super(&mut sb);
            }
            return Err(bch_errcode::BCH_ERR_cached_ptr_stale.into());
        }

        if b_gen != ptr_gen {
            let mem_gen = ca.bucket_gen(b, &read_lock()).map_or(u32::MAX, u32::from);
            return self.bucket_ref_update_err(k, inserting, id::stale_dirty_ptr, message!(
                "bucket {dev}:{b} gen {b_gen} (mem gen {mem_gen}) data type {}: stale dirty ptr (gen {ptr_gen})",
                type_str(*bucket_data_type)));
        }

        if bucket_data_type_mismatch(data_type_from_u8(*bucket_data_type), ptr_data_type) {
            self.bucket_ref_update_err(k, false, id::ptr_bucket_data_type_mismatch, message!(
                "bucket {dev}:{b} gen {b_gen} different types of data in same bucket: {}, {}",
                data_type_str(data_type_from_u8(*bucket_data_type)), data_type_str(ptr_data_type)))?;
            *bucket_data_type = data_type::multiple.0 as u8;
        }

        let new = u64::from(*bucket_sectors).wrapping_add(sectors as u64);
        if new > u64::from(u32::MAX) {
            return self.bucket_ref_update_err(k, inserting, id::bucket_sector_count_overflow, message!(
                "bucket {dev}:{b} gen {b_gen} data type {} sector count overflow: {} + {sectors} > U32_MAX",
                type_str(*bucket_data_type), *bucket_sectors));
        }

        *bucket_sectors = new as u32;
        Ok(())
    }

    /// A pointer that doesn't fit its bucket: @msg, counted as fsck error
    /// @err, and check_allocations scheduled to fix it. Inserting it - @fatal
    /// - is an inconsistency; overwriting it isn't an error at all, so
    /// deletion works.
    fn bucket_ref_update_err(&self, k: BkeySC<'_>, fatal: bool, err: c::bch_sb_error_id, msg: &dyn ToText)
        -> Result<(), BchError>
    {
        let fs = self.fs();
        let mut buf = LogMsg::new(fs);

        msg.to_text(&mut buf);
        prt!(&mut buf, "\nwhile marking {}\n", k.to_text(fs));

        let print = count_fsck_err(fs, err, &mut buf);
        let ret = passes::run_explicit(fs, &mut buf, c::bch_recovery_pass::BCH_RECOVERY_PASS_check_allocations,
                                       c::bch_run_recovery_pass_flags::from_bits_retain(0));

        let ret = if fatal {
            unsafe {
                c::bch2_trans_updates_to_text(buf.as_raw(), self.raw());
                c::__bch2_inconsistent_error(fs.as_ptr(), buf.as_raw());
            }
            // a recovery in progress can have it go back a pass instead
            ret.and_then(|()| fs.throw(bch_errcode::BCH_ERR_bucket_ref_update))
        } else {
            Ok(())
        };

        buf.set_suppressed(!(print || fatal));
        ret
    }
}

/// For C's bch2_bucket_ref_update(), past its checks.
///
/// # Safety
/// @trans is in an attempt.
#[c_export]
#[allow(clippy::too_many_arguments)]
pub unsafe fn __bch2_bucket_ref_update<'t>(
    trans:            &'t BtreeTrans<'t>,
    ca:               &c::bch_dev,
    k:                BkeySC<'_>,
    ptr:              &c::bch_extent_ptr,
    sectors:          i64,
    ptr_data_type:    c::bch_data_type,
    b_gen:            u8,
    bucket_data_type: &mut u8,
    bucket_sectors:   &mut u32,
) -> c_int {
    let t = unsafe { trans.attempt_in_progress() };
    ret_to_c(t.bucket_ref_update_slowpath(ca, k, ptr, sectors, ptr_data_type, b_gen,
                                          bucket_data_type, bucket_sectors))
}

// ── Triggers ─────────────────────────────────────────────────────────────

/// A pointer's sectors added to its bucket's alloc key @a - the stripe, dirty
/// or cached count, as the pointer is - and on insert, the bucket's data type
/// set from it: __mark_pointer().
#[allow(clippy::too_many_arguments)]
fn mark_pointer(t: &TransAttempt<'_, '_>, ca: &c::bch_dev, k: BkeySC<'_>, p: &c::extent_ptr_decoded,
                sectors: i64, ptr_data_type: c::bch_data_type, a: &mut c::AllocV4, insert: bool)
    -> Result<(), BchError>
{
    let (gen, mut bucket_data_type) = (a.generation, a.data_type);
    let dst = if p.has_ec                { &mut a.stripe_sectors }
              else if p.ptr.cached() == 0 { &mut a.dirty_sectors }
              else                         { &mut a.cached_sectors };
    let ret = t.bucket_ref_update(ca, k, &p.ptr, sectors, ptr_data_type, gen, &mut bucket_data_type, dst);
    // as C's, even on error: a mismatch it let through is BCH_DATA_multiple
    a.data_type = bucket_data_type;
    ret?;

    if insert {
        a.set_data_type(ptr_data_type);
    }
    Ok(())
}

/// A pointer to a device the filesystem doesn't have: reported, ratelimited,
/// and check_allocations scheduled - an error only if scheduling it is.
#[cold]
fn trigger_pointer_dev_missing(fs: &Fs, k: BkeySC<'_>, dev: u32, insert: bool) -> Result<(), BchError> {
    // C's: thrown, so counted and traced, then overwritten by what the
    // message returns
    if insert {
        let _ = fs.err(bch_errcode::BCH_ERR_trigger_pointer);
    }

    let mut msg = log_msg_ratelimited!(fs);
    prt!(&mut msg, "Error while {} key:\n", if insert { "inserting" } else { "deleting" });
    fs.dev_missing_bkey_msg(k, dev, &mut msg)
}

/// A pointer's bucket - its sectors and its backpointer - in the alloc key,
/// and in gc's copy with gc: bch2_trigger_pointer(). Returns the pointer's
/// sectors on disk, negative going out.
fn trigger_pointer(t: &TransAttempt<'_, '_>, btree: c::btree_id, level: u32, k: BkeySC<'_>,
                   d: &DecodedPtr<'_>, flags: UpdateTriggerFlags) -> Result<i64, BchError> {
    let fs = t.fs();
    let p = &d.p;
    let insert = !flags.contains(UpdateTriggerFlags::OVERWRITE);

    let mut bp = extent_ptr_to_bp(fs, btree, level, k, d);
    let bp_data_type = data_type_from_u8(bp.v.data_type);
    let sectors = if insert { i64::from(bp.v.bucket_len) } else { -i64::from(bp.v.bucket_len) };

    let dev = u32::from(p.ptr.dev());
    if dev == c::BCH_SB_MEMBER_INVALID {
        if flags.contains(UpdateTriggerFlags::TRANSACTIONAL) && p.has_ec {
            if !insert {
                bp.k.type_ = c::bch_bkey_type::KEY_TYPE_deleted.0 as u8;
                bp.k.set_val_bytes(0);
            }
            t.update_buffered(c::btree_id::stripe_backpointers, &t.bkey_copy(bp.as_ref())?)?;
        }
        return Ok(sectors);
    }

    let Some(ca) = fs.dev_get(dev) else {
        trigger_pointer_dev_missing(fs, k, dev, insert)?;
        return Ok(sectors);
    };

    let bucket = ca.ptr_bucket_pos(&p.ptr);
    if !ca.bucket_valid(bucket.offset) {
        if insert {
            ca.bucket_missing(bucket.offset);
            return fs.throw(bch_errcode::BCH_ERR_trigger_pointer);
        }
        return Ok(sectors);
    }

    if flags.contains(UpdateTriggerFlags::TRANSACTIONAL) {
        let mut a = t.start_alloc_update(bucket, UpdateTriggerFlags::empty())?;
        mark_pointer(t, &ca, k, p, sectors, bp_data_type, a.v_mut(), insert)?;
        bucket_backpointer_mod(t, k, &mut bp, insert)?;
    }

    if flags.contains(UpdateTriggerFlags::GC) {
        let g = ca.gc_bucket(bucket.offset).expect("gc has every valid bucket's");
        t.gc_bucket_mod(&ca, g, flags, |a| mark_pointer(t, &ca, k, p, sectors, bp_data_type, a, insert))?;
    }

    Ok(sectors)
}

/// An extent's erasure coded pointer, to a stripe it isn't part of - gone, or
/// with block pointers that don't match: logged, counted as an fsck error to
/// fix, and check_allocations scheduled to recount the stripe's blocks. Its
/// block isn't counted, rather than going emergency read-only.
#[cold]
fn mark_stripe_ptr_no_match(t: &TransAttempt<'_, '_>, k: BkeySC<'_>, idx: u64) -> Result<(), BchError> {
    let fs = t.fs();
    let mut msg = log_msg_ratelimited!(fs);

    prt!(&mut msg, "extent doesn't match stripe {idx}, not updating stripe block accounting\n  while marking {}\n",
         k.to_text(fs));
    count_fsck_err(fs, id::stripe_update_stale_stripe_ptr, &mut msg);
    passes::run_explicit(fs, &mut msg, c::bch_recovery_pass::BCH_RECOVERY_PASS_check_allocations,
                         c::bch_run_recovery_pass_flags::from_bits_retain(0))
}

/// An erasure coded pointer's sectors, in its block of the stripe - the key
/// in the stripes btree, and gc's copy with gc - and in the stripe's
/// replicas accounting: bch2_trigger_stripe_ptr().
fn trigger_stripe_ptr(t: &TransAttempt<'_, '_>, k: BkeySC<'_>, p: &c::extent_ptr_decoded,
                      data_type: c::bch_data_type, sectors: i64, flags: UpdateTriggerFlags)
    -> Result<(), BchError>
{
    let fs = t.fs();
    let (idx, block) = (p.ec.idx(), p.ec.block());

    if flags.contains(UpdateTriggerFlags::TRANSACTIONAL) {
        // the stripe, queued as its update: bch2_bkey_get_mut_typed()
        let mut iter = BtreeIter::new(t, c::btree_id::stripes, pos(0, idx),
                                      BtreeIterFlags::CACHED | BtreeIterFlags::INTENT);
        let s = match iter.peek_slot(t)?.and_then(|s| s.arm::<c::Stripe>()) {
            Some(s) => Some(t.bkey_make_mut_noupdate(s)?),
            None    => None,
        };
        if let Some(s) = &s {
            t.update(&iter, s, UpdateTriggerFlags::empty())?;
        }

        // gone, or not this extent's: either way, not part of the stripe
        let Some(mut s) = s.filter(|s| ptr_matches_stripe(s.as_sc(), p)) else {
            return mark_stripe_ptr_no_match(t, k, idx);
        };

        s.v().csum_type_check(fs)?;
        stripe_blockcount_add(&mut s, block.into(), sectors);

        let acct = bkey_replicas_acct(fs, s.as_sc().untyped(), data_type);
        return accounting::add(t, &acct, &[sectors], false);
    }

    if flags.contains(UpdateTriggerFlags::GC) {
        let Ok(m) = fs.ec.gc_stripe_alloc(idx, c::GFP_KERNEL) else {
            bch_err!(fs, "error allocating memory for gc_stripes, idx {idx}");
            return fs.throw(bch_errcode::BCH_ERR_ENOMEM_mark_stripe_ptr);
        };

        let acct = {
            let mut m = m.lock.lock();
            if m.alive() == 0 || !ptr_matches_stripe_m(&m, p) {
                drop(m);
                return mark_stripe_ptr_no_match(t, k, idx);
            }

            // as C's, wrapping to the u16
            let b = &mut m.block_sectors[usize::from(block)];
            *b = (i64::from(*b) + sectors) as u16;

            // SAFETY: the stripe's entry, as its trigger copied it in
            entry_acct(unsafe { &m.r.bytes }, data_type)
        };
        accounting::add(t, &acct, &[sectors], true)?;
    }

    Ok(())
}

/// One key's part of the extent trigger, going in or out by @flags: each
/// pointer's bucket and stripe, then the key's accounting - replicas,
/// compression, its snapshot's, and its btree's or inode's: __trigger_extent().
fn trigger_extent_key(t: &TransAttempt<'_, '_>, btree: c::btree_id, level: u32, k: BkeySC<'_>,
                      flags: UpdateTriggerFlags) -> Result<(), BchError> {
    let fs = t.fs();
    let gc = flags.contains(UpdateTriggerFlags::GC);
    let insert = !flags.contains(UpdateTriggerFlags::OVERWRITE);
    let nr: i64 = if insert { 1 } else { -1 };
    let data_type = if k.k.is_btree_ptr() { data_type::btree } else { data_type::user };

    let mut replicas_sectors = 0i64;
    let mut devs = [0u8; c::BCH_BKEY_PTRS_MAX as usize];
    let mut nr_devs = 0;
    let mut nr_required = 1;

    // [extents, uncompressed sectors, compressed sectors], for each run of
    // pointers of one compression type
    let compression_add = |ty: u8, mut v: [i64; 3]| {
        if !insert {
            v = v.map(i64::wrapping_neg);
        }
        accounting::add(t, &c::disk_accounting_pos::from_arm(c::bch_acct_compression { type_: ty }), &v, gc)
    };
    let mut compression_type = 0;
    let mut compression = [1, 0, 0];

    for d in k.arm::<Ptrs>().expect("a key with pointers").ptrs_decoded(fs) {
        let p = &d.p;
        let disk_sectors = match trigger_pointer(t, btree, level, k, &d, flags) {
            Ok(s) => s,
            // only a cached pointer is ever stale: it isn't counted
            Err(e) if e.matches(bch_errcode::BCH_ERR_cached_ptr_stale) => continue,
            Err(e) => return Err(e),
        };

        if p.ptr.cached() != 0 {
            mod_dev_cached_sectors(t, p.ptr.dev().into(), disk_sectors, gc)?;
        } else if !p.has_ec {
            replicas_sectors += disk_sectors;
            devs[nr_devs] = p.ptr.dev();
            nr_devs += 1;
        } else {
            trigger_stripe_ptr(t, k, p, data_type, disk_sectors, flags)?;
            // with an erasure coded pointer, its other dirty pointers aren't
            // needed to mount
            nr_required = 0;
        }

        if compression_type != 0 && compression_type != p.crc.compression_type {
            compression_add(compression_type, compression)?;
            compression = [1, 0, 0];
        }
        compression_type = p.crc.compression_type;
        if compression_type != 0 {
            compression[1] += i64::from(p.crc.uncompressed_size);
            compression[2] += i64::from(p.crc.compressed_size);
        }
    }

    if nr_devs != 0 {
        accounting::add(t, &replicas_acct(data_type, nr_required, &devs[..nr_devs]), &[replicas_sectors], gc)?;
    }

    // [keys, key bytes, sectors]: an extent counts its own key here, not by
    // trigger_snapshot_nr_keys(), as part of the sectors it adds anyway
    if level == 0 && k.k.p.snapshot != 0 {
        let acct = c::disk_accounting_pos::from_arm(c::bch_acct_snapshot { id: k.k.p.snapshot, btree: btree.0 });
        accounting::add(t, &acct, &[nr, nr * k.k.bytes() as i64, replicas_sectors], gc)?;
    }

    if compression_type != 0 {
        compression_add(compression_type, compression)?;
    }

    if level != 0 {
        // [sectors, nodes, interior nodes]
        let interior = if level > 1 { nr } else { 0 };
        let acct = c::disk_accounting_pos::from_arm(c::bch_acct_btree { id: btree.0 });
        accounting::add(t, &acct, &[replicas_sectors, nr, interior], gc)
    } else {
        // [extents, sectors, sectors on disk]
        let acct = c::disk_accounting_pos::from_arm(c::bch_acct_inum { inum: k.k.p.inode });
        accounting::add(t, &acct, &[nr, nr * i64::from(k.k.size), replicas_sectors], gc)
    }
}

/// The trigger of every key type with pointers: each key's pointers and
/// accounting - the old one's out, the new one's in - if its pointers
/// changed, and reconcile's part: bch2_trigger_extent().
pub(crate) fn trigger_extent(t: &TransAttempt<'_, '_>, op: &mut TriggerOp<'_>) -> Result<(), BchError> {
    {
        let (old, new) = (op.old.arm::<Ptrs>(), BkeySC::from(&op.new).untyped().arm::<Ptrs>());
        let old_entries = old.as_ref().map_or(&[][..], |k| k.entry_bytes());
        let new_entries = new.as_ref().map_or(&[][..], |k| k.entry_bytes());
        if old_entries == new_entries {
            return Ok(());
        }
    }

    if !op.flags.intersects(UpdateTriggerFlags::TRANSACTIONAL | UpdateTriggerFlags::GC) {
        return Ok(());
    }

    if op.flags.contains(UpdateTriggerFlags::TRANSACTIONAL | UpdateTriggerFlags::INSERT) &&
       !op.flags.contains(UpdateTriggerFlags::SET_NEEDS_RECONCILE_DONE) &&
       op.level == 0 && !op.new.k.is_deleted() {
        extent_trigger_set_needs_reconcile(t, op)?;
    }

    if !op.old.k.is_deleted() {
        trigger_extent_key(t, op.btree, op.level, op.old, op.flags - UpdateTriggerFlags::INSERT)?;
    }
    if !op.new.k.is_deleted() {
        trigger_extent_key(t, op.btree, op.level, BkeySC::from(&op.new).untyped(),
                           op.flags - UpdateTriggerFlags::OVERWRITE)?;
    }

    trigger_extent_reconcile(t, op)
}

crate::key_trigger!(bch2_trigger_extent => trigger_extent);

/// A reservation's sectors, by replica count - and in a snapshot, its key,
/// and the sectors it reserves at all its replicas: reservations carry no
/// pointers, so the snapshot's sectors, summed from extents' pointers, never
/// had them. Its key counted here, with them, as an extent's is, not by
/// trigger_snapshot_nr_keys(): bch2_trigger_reservation().
fn trigger_reservation(t: &TransAttempt<'_, '_>, op: &TriggerOp<'_>) -> Result<(), BchError> {
    if !op.flags.intersects(UpdateTriggerFlags::TRANSACTIONAL | UpdateTriggerFlags::GC) {
        return Ok(());
    }
    let gc = op.flags.contains(UpdateTriggerFlags::GC);

    op.run_overwrite_then_insert(|k, flags| {
        let nr_replicas = k.arm::<c::Reservation>().expect("a reservation").v().nr_replicas;
        let nr: i64 = if flags.contains(UpdateTriggerFlags::OVERWRITE) { -1 } else { 1 };
        let sectors = nr * i64::from(k.k.size);

        accounting::add(t, &c::disk_accounting_pos::from_arm(c::bch_acct_persistent_reserved { nr_replicas }),
                        &[sectors], gc)?;

        if op.level == 0 && k.k.p.snapshot != 0 {
            let acct = c::disk_accounting_pos::from_arm(c::bch_acct_snapshot { id: k.k.p.snapshot, btree: op.btree.0 });
            accounting::add(t, &acct, &[nr, nr * k.k.bytes() as i64, sectors * i64::from(nr_replicas)], gc)?;
        }
        Ok(())
    })
}

crate::key_trigger!(bch2_trigger_reservation => trigger_reservation);

/// Whether @k counts in its snapshot's keys: not a whiteout - iteration skips
/// those, so snapshot deletion never sees them - nor an extent or a
/// reservation, which count themselves.
fn snapshot_key_counted(k: &c::bkey) -> bool {
    k.p.snapshot != 0 &&
        !k.is_extent_whiteout() &&
        !k.is::<c::Extent>() &&
        !k.is::<c::Reservation>()
}

/// Each (snapshot, btree)'s leaf keys and their bytes, so snapshot deletion
/// can check a node is empty before splicing it out: the delta from @op's old
/// key to its new - the transactional dispatch passes a deleted key for the
/// side not running, the gc and mem dispatch both real keys at once. Not in
/// the atomic phase, which only applies deltas already staged:
/// bch2_trigger_snapshot_nr_keys().
fn trigger_snapshot_nr_keys(t: &TransAttempt<'_, '_>, op: &TriggerOp<'_>) -> Result<(), BchError> {
    if op.level != 0 || op.flags.contains(UpdateTriggerFlags::ATOMIC) {
        return Ok(());
    }

    let counted = |k: &c::bkey| snapshot_key_counted(k).then(|| k.bytes() as i64);
    let (new, old) = (counted(op.new.k), counted(op.old.k));

    let nr    = i64::from(new.is_some()) - i64::from(old.is_some());
    let bytes = new.unwrap_or(0) - old.unwrap_or(0);
    if nr == 0 && bytes == 0 {
        return Ok(());
    }

    // [nr_keys, key_bytes, external_sectors]: extents and reservations count sectors
    let acct = c::disk_accounting_pos::from_arm(c::bch_acct_snapshot { id: op.new.k.p.snapshot, btree: op.btree.0 });
    accounting::add(t, &acct, &[nr, bytes, 0], op.flags.contains(UpdateTriggerFlags::GC))
}

crate::key_trigger!(bch2_trigger_snapshot_nr_keys => trigger_snapshot_nr_keys);

// ── Metadata buckets ─────────────────────────────────────────────────────

/// Bucket @b of @ca as @sectors of @data_type in its alloc key - set, not
/// added to, so marking again is a no-op: __bch2_trans_mark_metadata_bucket().
fn trans_mark_metadata_bucket_key(t: &TransAttempt<'_, '_>, ca: &c::bch_dev, b: u64,
                                  data_type: c::bch_data_type, sectors: u32) -> Result<(), BchError> {
    let fs = t.fs();
    let mut iter = BtreeIter::new(t, c::btree_id::alloc, pos(ca.dev_idx.into(), b),
                                  BtreeIterFlags::CACHED | BtreeIterFlags::INTENT);
    let k = iter.peek_slot(t)?.expect("peek_slot() has a key at every position");
    let mut a = t.alloc_to_v4_mut(k)?;

    let cur = a.v().data_type;
    if cur != 0 && data_type.0 != 0 && u32::from(cur) != data_type.0 {
        let mut msg = LogMsg::new(fs);
        prt!(&mut msg, "bucket {}:{b} gen {} different types of data in same bucket: {}, {}\nwhile marking {}\n",
             ca.dev_idx, a.v().generation, data_type_str(data_type_from_u8(cur)),
             data_type_str(data_type), data_type_str(data_type));
        count_fsck_err(fs, id::bucket_metadata_type_mismatch, &mut msg);
        passes::run_explicit(fs, &mut msg, c::bch_recovery_pass::BCH_RECOVERY_PASS_check_allocations,
                             c::bch_run_recovery_pass_flags::from_bits_retain(0))?;
        return fs.throw(bch_errcode::BCH_ERR_metadata_bucket_inconsistency);
    }

    if u32::from(cur) != data_type.0 || a.v().dirty_sectors != sectors {
        a.v_mut().data_type     = data_type.0 as u8;
        a.v_mut().dirty_sectors = sectors;
        t.update(&iter, &a, UpdateTriggerFlags::empty())?;
    }
    Ok(())
}

/// trans_mark_metadata_bucket_key(), in gc's copy - which adds: gc counts
/// each bucket's metadata once: bch2_mark_metadata_bucket().
fn mark_metadata_bucket_gc(t: &TransAttempt<'_, '_>, ca: &c::bch_dev, b: u64, data_type: c::bch_data_type,
                           sectors: u32, flags: UpdateTriggerFlags) -> Result<(), BchError> {
    let fs = t.fs();
    let Some(g) = ca.gc_bucket(b) else {
        fs_inconsistent(fs, message!("reference to invalid bucket on device {} when marking metadata type {}",
                                     ca.dev_idx, data_type_str(data_type)));
        return fs.throw(bch_errcode::BCH_ERR_metadata_bucket_inconsistency);
    };

    t.gc_bucket_mod(ca, g, flags, |a| {
        let cur = data_type_from_u8(a.data_type);
        if a.data_type != 0 && cur != data_type {
            fs_inconsistent(fs, message!("different types of data in same bucket: {}, {}",
                                         data_type_str(cur), data_type_str(data_type)));
            return fs.throw(bch_errcode::BCH_ERR_metadata_bucket_inconsistency);
        }
        if u64::from(a.dirty_sectors) + u64::from(sectors) > u64::from(ca.mi.bucket_size) {
            fs_inconsistent(fs, message!(
                "bucket {}:{b} gen {} data type {} sector count overflow: {} + {sectors} > bucket size",
                ca.dev_idx, a.generation, data_type_str(if a.data_type != 0 { cur } else { data_type }),
                a.dirty_sectors));
            return fs.throw(bch_errcode::BCH_ERR_metadata_bucket_inconsistency);
        }

        a.data_type      = data_type.0 as u8;
        a.dirty_sectors += sectors;
        Ok(())
    })
}

/// Bucket @b of @ca holds @sectors of metadata - superblock, journal, or free
/// to unmark it: in its alloc key, committed, or in gc's copy:
/// bch2_trans_mark_metadata_bucket(). The gc side runs in an attempt of its
/// own; it's only reached on a transaction made for it.
pub fn trans_mark_metadata_bucket(trans: &BtreeTrans<'_>, ca: &c::bch_dev, b: u64, data_type: c::bch_data_type,
                                  sectors: u32, flags: UpdateTriggerFlags) -> Result<(), BchError> {
    assert!(matches!(data_type, data_type::free | data_type::sb | data_type::journal),
            "marking {} as metadata", data_type_str(data_type));

    // a backup superblock can be past the end of the usable space
    if b >= ca.mi.nbuckets {
        return Ok(());
    }

    if flags.contains(UpdateTriggerFlags::GC) {
        mark_metadata_bucket_gc(&trans.begin(), ca, b, data_type, sectors, flags)
    } else if flags.contains(UpdateTriggerFlags::TRANSACTIONAL) {
        commit_do(trans, None, CommitOpts::new().watermark(Watermark::BTREE),
                  |t| trans_mark_metadata_bucket_key(t, ca, b, data_type, sectors))
    } else {
        panic!("marking metadata, neither gc nor transactional");
    }
}

/// @range's sectors on @ca, metadata of @data_type, into @bucket - a bucket
/// and its count so far - marking the bucket when the range moves past it:
/// bch2_trans_mark_metadata_sectors().
fn trans_mark_metadata_sectors(trans: &BtreeTrans<'_>, ca: &c::bch_dev, range: core::ops::Range<u64>,
                               data_type: c::bch_data_type, bucket: &mut (u64, u32),
                               flags: UpdateTriggerFlags) -> Result<(), BchError> {
    let mut start = range.start;
    loop {
        let b = ca.sector_to_bucket(start);
        let sectors = (ca.bucket_to_sector(b + 1).min(range.end) - start) as u32;

        if b != bucket.0 && bucket.1 != 0 {
            trans_mark_metadata_bucket(trans, ca, bucket.0, data_type, bucket.1, flags)?;
            bucket.1 = 0;
        }

        bucket.0  = b;
        bucket.1 += sectors;
        start    += u64::from(sectors);
        if start >= range.end {
            return Ok(());
        }
    }
}

/// @ca's superblocks' buckets - the first's, from sector 0 - and its journal
/// buckets: __bch2_trans_mark_dev_sb().
fn trans_mark_dev_sb_in(trans: &BtreeTrans<'_>, ca: &c::bch_dev, flags: UpdateTriggerFlags)
    -> Result<(), BchError>
{
    let fs = trans.fs();
    let layout = {
        let sb = fs.sb_lock();
        fs.dev_sb(&sb, ca).expect("marking a device with no superblock").sb().layout
    };
    let sb_sectors = 1u64 << layout.sb_max_size_bits;
    let sb_sector = u64::from(c::BCH_SB_SECTOR);
    let mut bucket = (0, 0);

    for offset in layout.sb_offset[..usize::from(layout.nr_superblocks)].iter().map(|o| o.get()) {
        if offset == sb_sector {
            trans_mark_metadata_sectors(trans, ca, 0..sb_sector, data_type::sb, &mut bucket, flags)?;
        }
        trans_mark_metadata_sectors(trans, ca, offset..offset + sb_sectors, data_type::sb, &mut bucket, flags)?;
    }
    if bucket.1 != 0 {
        trans_mark_metadata_bucket(trans, ca, bucket.0, data_type::sb, bucket.1, flags)?;
    }

    for &b in ca.journal.buckets() {
        trans_mark_metadata_bucket(trans, ca, b, data_type::journal, ca.mi.bucket_size.into(), flags)?;
    }
    Ok(())
}

impl Fs {
    /// Device @ca's superblock and journal buckets marked as theirs, in the
    /// alloc btree - or gc's copy, with gc: bch2_trans_mark_dev_sb().
    pub fn trans_mark_dev_sb(&self, ca: &c::bch_dev, flags: UpdateTriggerFlags) -> Result<(), BchError> {
        let trans = btree_trans!(self);
        bch_err_fn!(self, trans_mark_dev_sb_in(&trans, ca, flags))
    }

    /// trans_mark_dev_sb() of every online device - each initialized first,
    /// if it wasn't: bch2_trans_mark_dev_sbs_flags().
    pub fn trans_mark_dev_sbs(&self, flags: UpdateTriggerFlags) -> Result<(), BchError> {
        for ca in self.online_members(c::bch_dev_read_ref::BCH_DEV_READ_REF_trans_mark_dev_sbs) {
            // marked again, even if it was: harmless if not needed, and a
            // superblock or journal bucket allocated twice would be painful
            let ca_raw = core::ptr::from_ref::<c::bch_dev>(&ca).cast_mut();
            ret_to_result(unsafe { c::bch2_dev_add_initialize(self.as_ptr(), ca_raw) })?;
            self.trans_mark_dev_sb(&ca, flags)?;
        }
        Ok(())
    }
}

#[c_export]
pub fn bch2_trans_mark_metadata_bucket<'t>(trans: &'t BtreeTrans<'t>, ca: &c::bch_dev, b: u64,
                                           data_type: c::bch_data_type, sectors: c_uint,
                                           flags: c::btree_iter_update_trigger_flags) -> c_int {
    ret_to_c(trans_mark_metadata_bucket(trans, ca, b, data_type, sectors,
                                        UpdateTriggerFlags::from_bits_retain(flags.bits())))
}

#[c_export]
pub fn bch2_trans_mark_dev_sb(c: &Opaque<c::bch_fs>, ca: &c::bch_dev, flags: c::btree_iter_update_trigger_flags)
    -> c_int
{
    ret_to_c(Fs::from_c(c).trans_mark_dev_sb(ca, UpdateTriggerFlags::from_bits_retain(flags.bits())))
}

#[c_export]
pub fn bch2_trans_mark_dev_sbs_flags(c: &Opaque<c::bch_fs>, flags: c::btree_iter_update_trigger_flags) -> c_int {
    ret_to_c(Fs::from_c(c).trans_mark_dev_sbs(UpdateTriggerFlags::from_bits_retain(flags.bits())))
}

#[c_export]
pub fn bch2_trans_mark_dev_sbs(c: &Opaque<c::bch_fs>) -> c_int {
    ret_to_c(Fs::from_c(c).trans_mark_dev_sbs(UpdateTriggerFlags::TRANSACTIONAL))
}

// ── Startup and shutdown ─────────────────────────────────────────────────

impl Fs {
    /// Every device's buckets_nouse, a zeroed bit per bucket: for migrate, to
    /// mark the space it can't use. bch2_buckets_nouse_alloc().
    pub fn buckets_nouse_alloc(&self) -> Result<(), BchError> {
        for ca in self.members() {
            assert!(ca.buckets_nouse.load(Ordering::Relaxed).is_null(), "buckets_nouse allocated twice");

            let p = unsafe { c::rust_kvzalloc(bits_to_longs(ca.mi.nbuckets as usize) * size_of::<c_ulong>()) };
            if p.is_null() {
                return self.throw(bch_errcode::BCH_ERR_ENOMEM_buckets_nouse);
            }
            ca.buckets_nouse.store(p.cast(), Ordering::Release);
        }
        Ok(())
    }
}

impl c::bch_dev {
    /// Its buckets' generations at @nbuckets - as many of the old copied as
    /// both have - and its bucket bitmaps with them; nothing changed on
    /// failure. A resize is under state_lock: bch2_dev_buckets_resize().
    pub fn buckets_resize(&self, nbuckets: u64) -> Result<(), BchError> {
        let fs = self.fs();
        // SAFETY: we're the writer - at init, or under state_lock
        let old = unsafe { self.bucket_gens.get_head_locked() }.map(|(_, gens)| gens);

        if old.is_some() {
            Lock::from_c(&fs.state_lock).assert_held_any();
            if !self.buckets_nouse.load(Ordering::Relaxed).is_null() {
                return fs.throw(bch_errcode::BCH_ERR_no_resize_with_buckets_nouse);
            }
        }

        let head = c::BucketGensHead {
            first_bucket:         self.mi.first_bucket,
            nbuckets_minus_first: (nbuckets - u64::from(self.mi.first_bucket)) as usize,
        };
        // SAFETY: as above
        unsafe {
            self.bucket_gens.rebuild_head(nbuckets as usize, head, |gens| {
                if let Some(old) = old {
                    let n = gens.len().min(old.len());
                    gens[..n].copy_from_slice(&old[..n]);
                }
                self.bucket_backpointer_mismatch.resize(self, nbuckets)?;
                self.bucket_backpointer_empty.resize(self, nbuckets)
            })
        }.or_else(|_| fs.throw(bch_errcode::BCH_ERR_ENOMEM_bucket_gens))?
    }

    /// Its usage counters and bucket generations, as it's set up:
    /// bch2_dev_buckets_alloc().
    pub fn buckets_alloc(&mut self) -> Result<(), BchError> {
        match PerCpu::alloc() {
            Ok(u)  => self.usage = u,
            Err(_) => return self.fs().throw(bch_errcode::BCH_ERR_ENOMEM_usage_init),
        }
        self.buckets_resize(self.mi.nbuckets)
    }

    /// What buckets_alloc() and buckets_nouse_alloc() allocated, as it's
    /// freed: bch2_dev_buckets_free().
    pub fn buckets_free(&mut self) {
        unsafe {
            c::rust_kvfree(self.buckets_nouse.swap(core::ptr::null_mut(), Ordering::Relaxed).cast());
            self.bucket_gens.free();
        }
        self.usage.free();
    }
}

#[c_export]
pub fn bch2_buckets_nouse_alloc(c: &Opaque<c::bch_fs>) -> c_int {
    ret_to_c(Fs::from_c(c).buckets_nouse_alloc())
}

#[c_export]
pub fn bch2_dev_buckets_resize(_c: &Opaque<c::bch_fs>, ca: &c::bch_dev, nbuckets: u64) -> c_int {
    ret_to_c(ca.buckets_resize(nbuckets))
}

/// @ca is being set up: nothing else has it yet.
#[c_export]
pub fn bch2_dev_buckets_alloc(_c: &Opaque<c::bch_fs>, ca: &mut c::bch_dev) -> c_int {
    ret_to_c(ca.buckets_alloc())
}

/// @ca is being freed: nothing else has it any more.
#[c_export]
pub fn bch2_dev_buckets_free(ca: &mut c::bch_dev) {
    ca.buckets_free()
}

// ── Usage ────────────────────────────────────────────────────────────────

impl c::bch_dev {
    /// Its buckets by data type, every CPU's count summed - not under any
    /// lock, so only near enough: bch2_dev_usage_read_fast().
    pub fn usage_read_fast(&self) -> c::bch_dev_usage {
        // The allocator's: one sweep over the CPUs, reading only the bucket
        // counts, a third of each CPU's usage
        let mut u = c::bch_dev_usage::default();
        for cpu in self.usage.get().cpus() {
            for (i, b) in u.buckets.iter_mut().enumerate() {
                *b = b.wrapping_add(percpu_field!(cpu, d[i].buckets).get());
            }
        }
        u.clamp_nonnegative()
    }

    /// All of its usage, as usage_read_fast(): bch2_dev_usage_full_read_fast().
    pub fn usage_full_read_fast(&self) -> c::bch_dev_usage_full {
        self.usage.sum_nonnegative()
    }

    /// @usage, of this device, as a table: bch2_dev_usage_to_text().
    pub fn usage_to_text(&self, out: &mut Printbuf, usage: &c::bch_dev_usage_full) {
        if out.nr_tabstops() < 5 {
            out.tabstops_reset();
            for spaces in [12, 16, 16, 16, 16] {
                out.tabstop_push(spaces);
            }
        }

        write!(out, "\tbuckets\rsectors\rfragmented\r\n");
        for (i, u) in usage.d.iter().enumerate() {
            prt_data_type(out, data_type_from_u8(i as u8));
            write!(out, "\t{}\r{}\r{}\r\n", u.buckets, u.sectors, u.fragmented);
        }
        write!(out, "capacity\t{}\r\n", self.mi.nbuckets);
    }
}

/// @r, and the margin charged on top of a reservation: reserve_factor().
fn reserve_factor(r: u64) -> u64 {
    r + (r.next_multiple_of(1 << c::RESERVE_FACTOR) >> c::RESERVE_FACTOR)
}

impl Fs {
    /// Sectors in use by kind, every CPU's count summed:
    /// bch2_fs_usage_base_read().
    pub fn usage_base(&self) -> c::bch_fs_usage_base {
        percpu_field!(self.capacity.pcpu.get(), usage).sum_nonnegative()
    }

    /// Sectors reserved for writes in flight, every CPU's count summed:
    /// bch2_online_reserved().
    pub fn online_reserved(&self) -> u64 {
        percpu_field!(self.capacity.pcpu.get(), online_reserved).sum_nonnegative().iter().sum()
    }

    /// Capacity, and how much of it is used and free, in sectors:
    /// bch2_fs_usage_read_short().
    pub fn usage_read_short(&self) -> c::bch_fs_usage_short {
        let _mark = self.mark_lock().read();
        // one sweep over the CPUs: sectors_available is summed too, and dropped
        let b = self.capacity.pcpu.sum_nonnegative();

        // a u64's, as C's: nothing keeps hidden below capacity
        let capacity = self.capacity.capacity.wrapping_sub(b.usage.hidden);
        let data     = b.usage.data + b.usage.btree;
        // outstanding reservations are counted by replicas; all of them
        let reserved = b.usage.reserved + b.online_reserved.iter().sum::<u64>();
        let used     = capacity.min(data + reserve_factor(reserved));

        c::bch_fs_usage_short { capacity, used, free: capacity - used }
    }
}

/// # Safety
/// @usage is writable.
#[c_export]
pub unsafe fn bch2_dev_usage_read_fast(ca: &c::bch_dev, usage: *mut c::bch_dev_usage) {
    unsafe { usage.write(ca.usage_read_fast()) }
}

/// # Safety
/// @usage is writable.
#[c_export]
pub unsafe fn bch2_dev_usage_full_read_fast(ca: &c::bch_dev, usage: *mut c::bch_dev_usage_full) {
    unsafe { usage.write(ca.usage_full_read_fast()) }
}

#[cold]
#[c_export]
pub fn bch2_dev_usage_to_text(out: &mut Printbuf, ca: &c::bch_dev, usage: &c::bch_dev_usage_full) {
    ca.usage_to_text(out, usage)
}

#[c_export]
pub fn bch2_fs_usage_base_read(c: &Opaque<c::bch_fs>) -> c::bch_fs_usage_base {
    Fs::from_c(c).usage_base()
}

#[c_export]
pub fn bch2_fs_usage_read_short(c: &Opaque<c::bch_fs>) -> c::bch_fs_usage_short {
    Fs::from_c(c).usage_read_short()
}

/// The bucket @v encodes - device in the top 16 bits, bucket in the low 48 -
/// as the lru btree's targets and need_discard's positions do:
/// u64_to_bucket().
pub fn u64_to_bucket(v: u64) -> c::bpos {
    pos(v >> 48, v & ((1 << 48) - 1))
}

/// u64_to_bucket()'s inverse: bucket_to_u64().
pub fn bucket_to_u64(bucket: c::bpos) -> u64 {
    bucket.inode << 48 | (bucket.offset & ((1 << 48) - 1))
}

// ── Bucket generations ───────────────────────────────────────────────────

/// How far generation @a is past @b - negative if it's before - wrapping, as
/// gens do: gen_cmp().
pub fn gen_cmp(a: u8, b: u8) -> i8 {
    a.wrapping_sub(b) as i8
}

/// gen_cmp(), or 0 if @a isn't past @b: gen_after().
pub fn gen_after(a: u8, b: u8) -> u8 {
    gen_cmp(a, b).max(0) as u8
}

/// A device's bucket generations, under RCU: alloc/buckets.h's.
impl c::bch_dev {
    /// Bucket @b's generation, for as long as @rcu's section - None if it
    /// isn't one of the device's: bucket_gen_get_rcu().
    pub fn bucket_gen(&self, b: u64, rcu: &ReadGuard) -> Option<u8> {
        let (h, gens) = self.bucket_gens.read_head(rcu)?;
        (b.wrapping_sub(h.first_bucket.into()) < h.nbuckets_minus_first as u64)
            .then(|| gens.get(b as usize).copied())
            .flatten()
    }

    /// How many generations @ptr's bucket has moved on from it - None for a
    /// bucket the device doesn't have: dev_ptr_stale_rcu(), which is -1 there.
    pub fn ptr_stale(&self, ptr: &c::bch_extent_ptr, rcu: &ReadGuard) -> Option<u8> {
        Some(gen_after(self.bucket_gen(self.ptr_bucket(ptr), rcu)?, ptr.generation()))
    }
}

/// A disk space reservation.
///
/// The raw reservation lives behind an `UnsafeCell`: the commit path mutates it
/// (in C) through the pointer installed on the transaction, while the holder may
/// still inspect or grow it — shared, aliased, mutated-by-the-callee. `&mut`
/// would falsely claim exclusivity the commit doesn't honor, so the mutating
/// methods take `&self` and a reservation can be lent to `commit_do` (by shared
/// ref) across many commits while the caller retains it.
pub struct DiskReservation<'f> {
    fs:  &'f Fs,
    raw: UnsafeCell<c::disk_reservation>,
}

impl<'f> DiskReservation<'f> {
    pub fn new(fs: &'f Fs) -> Self {
        DiskReservation {
            fs,
            raw: UnsafeCell::new(Default::default()),
        }
    }

    /// Reserve `sectors` of data at `nr_replicas` copies, on top of whatever
    /// this reservation already holds.
    pub fn add(
        &self,
        sectors:     u64,
        nr_replicas: u32,
        flags:       c::bch_reservation_flags,
    ) -> Result<(), BchError> {
        ret_to_result(unsafe {
            c::rust_bch2_disk_reservation_add(
                self.fs.as_ptr(),
                self.raw.get(),
                sectors,
                nr_replicas,
                flags.bits() as i32,
            )
        })
    }

    /// Release what this reservation holds, keeping it usable: as
    /// bch2_disk_reservation_put(). For a loop reserving per iteration.
    pub fn put(&self) {
        unsafe { c::rust_bch2_disk_reservation_put(self.fs.as_ptr(), self.raw.get()) };
    }

    pub fn as_ptr(&self) -> *const c::disk_reservation {
        self.raw.get()
    }

    pub fn as_mut_ptr(&self) -> *mut c::disk_reservation {
        self.raw.get()
    }

    pub fn sectors(&self) -> u64 {
        unsafe { (*self.raw.get()).sectors }
    }
}

impl Drop for DiskReservation<'_> {
    fn drop(&mut self) {
        self.put();
    }
}
