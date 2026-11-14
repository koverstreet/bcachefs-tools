// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::init::passes::RecoveryPass;
use crate::fs::{Dev, StateLockWrite};
use crate::accounting::{self, mod_dev_cached_sectors};
use crate::alloc::backpointers::{bucket_backpointer_mod, extent_ptr_to_bp};
use crate::alloc::replicas::{bkey_replicas_acct, entry_acct, replicas_acct};
use crate::btree::bkey::prelude::*;
use crate::btree::bkey::{pos, BkeySC};
use crate::btree::trigger::{Trigger, TriggerOp};
use crate::btree::iter::{commit_do, lockrestart_do, BtreeIter, BtreeIterFlags, CommitOpts, TransAttempt,
                         UpdateTriggerFlags, Watermark};
use crate::btree::trans::TransGuard;
use crate::c;
use crate::c::Errcode;
use crate::data::ec::stripe::{ptr_matches_stripe, ptr_matches_stripe_m};
use crate::data::ptrs::{BkeyVPtrs, DecodedPtr, Ptrs};
use crate::data::reconcile::needs_reconcile::extent_trigger_set_needs_reconcile;
use crate::data::reconcile::trigger::trigger_extent_reconcile;
use crate::errcode::BchError;
use crate::fs::Fs;
use crate::init::error::{count_fsck_err, fs_inconsistent, id};
use crate::init::passes;
use crate::opts::data_type_str;
use crate::locking::{ByteLock, Guard as _, Locked};
use crate::sb::errors::SbErrorId;
use crate::types::bits_to_longs;
use crate::os::kernel::unlikely;
use crate::util::log::LogMsg;
use crate::util::percpu::{Counters, PerCpu};
use crate::util::rcu::{read_lock, RcuArrayWriter, ReadGuard};
use crate::data_structures::bitmap::AtomicBitmap;
use crate::util::{Printbuf, ToText};
use crate::{bch_err, bch_err_fn, btree_trans, fsck_err_on, log_msg_ratelimited, message, percpu_field, prt};
use core::convert::Infallible;
use core::ffi::c_uint;
use core::sync::atomic::{AtomicU8, Ordering};
use cstruct_macros::{MaybeZeroable, bitfield, Fieldwise, TypeInfo, bch_enum, BitfieldMembers};
use core::cell::Cell;
use nestify::nest;
use crate::format::{Bpos, Btree, CompressionType, DataType, BkeyType};

#[bch_enum(flags)]
#[repr(u32)]
pub enum ReservationFlags {
    BCH_DISK_RESERVATION_NOFAIL = 1 << 0,
    BCH_DISK_RESERVATION_PARTIAL = 1 << 1,
}

#[repr(C)]
#[derive(MaybeZeroable)]
pub struct disk_reservation_destructable {
    pub r: c::DiskReservation<'static>,
}

pub const RESERVE_FACTOR: u32 = 6;

/* A device position as a 32.32 fixed point fraction of its size: see
 * bch_dev::offset_to_frac() */
pub const BCH_DEV_POS_FRAC_BITS: u32 = 32;

/// A bucket's gc state, in memory: fsck has one for every bucket on every
/// device, so it's 16 bytes, the lock one of them.
#[repr(C, align(4))]
#[derive(MaybeZeroable)]
pub struct bucket {
    pub lock: Locked<ByteLock, BucketData>,
}

nest! {
    /// What a bucket's lock protects. Packed, so it starts right after the
    /// lock byte; laid out so the u32s are aligned all the same.
    #[repr(C, packed)]
    #[derive(Clone, Copy, Default, MaybeZeroable, BitfieldMembers)]
    pub struct BucketData {
        #[members]
        #>[bitfield(u8)]
        pub bits: pub struct bucket_bits {
            gen_valid: u1,
            #[bits(7)] data_type: c::DataType,
        },
        pub generation: u8,
        pub __pad: u8,
        pub dirty_sectors: u32,
        pub cached_sectors: u32,
        pub stripe_sectors: u32,
    }
}

/// The header of a device's bucket generations: C's bucket_gens fields
/// between nbuckets and b[].
#[repr(C)]
#[derive(MaybeZeroable)]
pub struct BucketGensHead {
    pub first_bucket: u16,
    pub nbuckets_minus_first: usize,
}

// SAFETY: integers; all zeroes is an empty table
unsafe impl crate::util::rcu::RcuElement for BucketGensHead {}

/// ca->bucket_gens: a device's bucket generations, published under RCU -
/// atomics, written in place with readers looking.
pub type BucketGensArray = crate::util::rcu::RcuArray<core::sync::atomic::AtomicU8, c::BucketGensHead>;

/* Only info on bucket countns: */
#[repr(C)]
#[derive(Clone, Copy, Default, MaybeZeroable, TypeInfo, Fieldwise)]
#[fieldwise(Counters::wrapping_add(rhs), Counters::clamp_nonnegative)]
pub struct bch_dev_usage {
    pub buckets: [u64; c::DataType::NR as usize],
}

#[repr(C)]
#[derive(Clone, Copy, Default, MaybeZeroable, TypeInfo, Fieldwise)]
#[fieldwise(Counters::wrapping_add(rhs), Counters::clamp_nonnegative)]
pub struct bch_dev_usage_type {
    pub buckets: u64,
    pub sectors: u64, /* _compressed_ sectors: */
    /*
     * XXX
     * Why do we have this? Isn't it just buckets * bucket_size -
     * sectors?
     */
    pub fragmented: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default, MaybeZeroable, TypeInfo, Fieldwise)]
#[fieldwise(Counters::wrapping_add(rhs), Counters::clamp_nonnegative)]
pub struct bch_dev_usage_full {
    pub d: [c::bch_dev_usage_type; c::DataType::NR as usize],
}

#[repr(C)]
#[derive(Clone, Copy, Default, MaybeZeroable, TypeInfo, Fieldwise)]
#[fieldwise(Counters::wrapping_add(rhs), Counters::clamp_nonnegative)]
pub struct bch_fs_usage_base {
    pub hidden: u64,
    pub btree: u64,
    pub data: u64,
    pub cached: u64,
    pub reserved: u64,
}

impl bch_fs_usage_base {
    pub fn add_data_type(&mut self, data_type: DataType, sectors: u64) {

        let to = match data_type {
            DataType::btree => &mut self.btree,
            DataType::user | DataType::parity => &mut self.data,
            DataType::cached => &mut self.cached,
            _ => return,
        };
        *to = to.wrapping_add(sectors);
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default, MaybeZeroable, TypeInfo)]
pub struct bch_fs_usage_short {
    pub capacity: u64,
    pub used: u64,
    pub free: u64,
}

/* Free space by the replica count it's placeable at: [n - 1] at n or more */
#[repr(C)]
#[derive(Clone, Copy, Default, MaybeZeroable, TypeInfo)]
pub struct bch_fs_placeable {
    /* waiting on copygc, if need be */
    pub eventual: [u64; c::BCH_REPLICAS_MAX as usize],
    /* free buckets only */
    pub now: [u64; c::BCH_REPLICAS_MAX as usize],
}

/*
 * A reservation for space on disk. @sectors is physical - total disk space, not
 * the size of the data - and is charged to the online_reserved[] slot named by
 * @nr_replicas: charged sectors always have one, and changing it means moving
 * them, with disk_res_move_slot().
 *
 * Lent to a commit, which changes it through the transaction's pointer while
 * its holder has it shared - so its fields are Cells.
 *
 * @c is the filesystem it reserves on, set where it's made, so a reservation
 * releases what it holds when it's dropped. Rust has no zeroed one: nothing
 * that embeds one is Default.
 */
#[repr(C)]
#[derive(MaybeZeroable)]
pub struct DiskReservation<'f> {
    pub c: &'f Fs,
    pub sectors: Cell<u64>,
    pub generation: Cell<u32>,
    pub nr_replicas: Cell<c_uint>,
}

// ── Bucket addressing ────────────────────────────────────────────────────

/// A device's sectors as its buckets: alloc/buckets.h's inlines.
impl Dev {
    /// sector_to_bucket().
    pub fn sector_to_bucket(&self, s: u64) -> u64 {
        s / u64::from(self.mi.get().bucket_size)
    }

    /// bucket_to_sector().
    pub fn bucket_to_sector(&self, b: u64) -> u64 {
        b * u64::from(self.mi.get().bucket_size)
    }

    /// Whether it counts toward c->capacity: rw, with durability -
    /// buckets.h's dev_has_capacity(). Whatever divides capacity up per
    /// device must agree on which do.
    pub fn has_capacity(&self) -> bool {
        self.rw_durability() != 0
    }

    /// Its buckets, in sectors - past first_bucket.
    pub fn capacity_sectors(&self) -> u64 {
        self.bucket_to_sector(self.mi.get().nbuckets - u64::from(self.mi.get().first_bucket))
    }

    /// Whether the allocator mustn't use bucket @b - migrate marks the space
    /// it can't: bch2_bucket_nouse().
    pub fn bucket_nouse(&self, b: u64) -> bool {
        self.buckets_nouse.with(|w| AtomicBitmap::from_words(w).test(b as usize))
    }

    /// Whether bucket @b holds a superblock - bucket 0 does, from sector 0 to
    /// the first - or journal. From sb_layout, as the allocator can't take
    /// sb_lock.
    pub fn is_superblock_bucket(&self, b: u64) -> bool {
        let l = self.sb_layout.get();
        let sb_sectors = 1u64 << l.sb_max_size_bits;
        let (start, end) = (self.bucket_to_sector(b), self.bucket_to_sector(b + 1));

        b == 0 ||
            l.sb_offset.iter().take(usize::from(l.nr_superblocks))
                .any(|o| o.get() < end && start < o.get() + sb_sectors) ||
            self.with_journal_ring(|r| r.owns(b))
    }

    /// The bucket sector @s is in, and how far into it:
    /// sector_to_bucket_and_offset().
    pub fn sector_to_bucket_and_offset(&self, s: u64) -> (u64, u32) {
        let size = u64::from(self.mi.get().bucket_size);
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
    pub fn ptr_bucket_pos(&self, ptr: &c::bch_extent_ptr) -> Bpos {
        pos(ptr.dev().into(), self.ptr_bucket(ptr))
    }

    /// Sector @offset as a fraction of the device - 32.32 fixed point - for
    /// comparing and averaging positions on devices of different sizes, where
    /// the same fraction is about the same zone of rotational media.
    pub fn offset_to_frac(&self, offset: u64) -> u64 {
        let size = u128::from(self.bucket_to_sector(self.mi.get().nbuckets));
        ((u128::from(offset) << c::BCH_DEV_POS_FRAC_BITS) / size) as u64
    }

    /// offset_to_frac()'s inverse: dev_frac_to_offset().
    pub fn frac_to_offset(&self, frac: u64) -> u64 {
        let size = u128::from(self.bucket_to_sector(self.mi.get().nbuckets));
        ((u128::from(frac) * size) >> c::BCH_DEV_POS_FRAC_BITS) as u64
    }
}

// ── GC buckets ───────────────────────────────────────────────────────────

impl Dev {
    /// Bucket @b's gc state, if it's one of the device's: gc_bucket().
    pub fn gc_bucket(&self, b: u64) -> Option<&c::bucket> {
        self.bucket_valid(b).then(|| self.buckets_gc.get(b as usize)).flatten()
    }
}

impl TransAttempt<'_, '_> {
    /// Gc bucket @g's state, as an alloc key, edited by @f under its lock -
    /// left as it was if @f fails - and device @ca's counters moved from the
    /// old to the new after: a trigger's gc side, on a bucket.
    pub fn gc_bucket_mod(&self, ca: &Dev, g: &c::bucket, flags: UpdateTriggerFlags,
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
        a.data_type.set(self.data_type());
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
        self.set_data_type(a.data_type());
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
    pub fn bucket_ref_update(&self, ca: &Dev, k: BkeySC<'_>, ptr: &c::bch_extent_ptr,
                             sectors: i64, ptr_data_type: DataType, b_gen: u8,
                             bucket_data_type: &mut DataType, bucket_sectors: &mut u32) -> Result<(), BchError> {
        assert!(sectors != 0);

        // as C's: a negative @sectors past the count wraps, and is past U32_MAX
        let new = u64::from(*bucket_sectors).wrapping_add(sectors as u64);
        if b_gen != ptr.generation() ||
           bucket_data_type.mismatches(ptr_data_type) ||
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
    fn bucket_ref_update_slowpath(&self, ca: &Dev, k: BkeySC<'_>, ptr: &c::bch_extent_ptr,
                                  sectors: i64, ptr_data_type: DataType, b_gen: u8,
                                  bucket_data_type: &mut DataType, bucket_sectors: &mut u32) -> Result<(), BchError> {
        let fs = self.fs();
        let (dev, b, ptr_gen) = (ptr.dev(), ca.ptr_bucket(ptr), ptr.generation());
        let inserting = sectors > 0;
        // the bucket's type, or the pointer's if it hasn't one
        let type_str = |t: DataType| data_type_str(if t != DataType::free { t } else { ptr_data_type });

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
            if fsck_err_on!(self, fs.compat(c::SbCompat::no_stale_ptrs),
                            id::stale_ptr_with_no_stale_ptrs_feature,
                            "stale cached ptr, but have no_stale_ptrs feature\n{}", k.to_text(fs))? {
                let mut sb = fs.sb_lock();
                sb.sb_mut().compat.update(|f| f - c::SbCompats::no_stale_ptrs);
                fs.write_super(&mut sb);
            }
            return Err(Errcode::BCH_ERR_cached_ptr_stale.into());
        }

        if b_gen != ptr_gen {
            let mem_gen = ca.bucket_gen(b, &read_lock()).map_or(u32::MAX, u32::from);
            return self.bucket_ref_update_err(k, inserting, id::stale_dirty_ptr, message!(
                "bucket {dev}:{b} gen {b_gen} (mem gen {mem_gen}) data type {}: stale dirty ptr (gen {ptr_gen})",
                type_str(*bucket_data_type)));
        }

        if bucket_data_type.mismatches(ptr_data_type) {
            self.bucket_ref_update_err(k, false, id::ptr_bucket_data_type_mismatch, message!(
                "bucket {dev}:{b} gen {b_gen} different types of data in same bucket: {}, {}",
                data_type_str(*bucket_data_type), data_type_str(ptr_data_type)))?;
            *bucket_data_type = DataType::multiple;
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
    fn bucket_ref_update_err(&self, k: BkeySC<'_>, fatal: bool, err: SbErrorId, msg: &dyn ToText)
        -> Result<(), BchError>
    {
        let fs = self.fs();
        let mut buf = LogMsg::new(fs);

        msg.to_text(&mut buf);
        prt!(&mut buf, "\nwhile marking {}\n", k.to_text(fs));

        let print = count_fsck_err(fs, err, &mut buf);
        let ret = passes::run_explicit(fs, &mut buf, RecoveryPass::check_allocations,
                                       c::RunRecoveryPassFlags::from_bits_retain(0));

        let ret = if fatal {
            self.updates_to_text(&mut buf);
            fs.inconsistent_error(&mut buf);
            // a recovery in progress can have it go back a pass instead
            ret.and_then(|()| fs.throw(Errcode::BCH_ERR_bucket_ref_update))
        } else {
            Ok(())
        };

        buf.set_suppressed(!(print || fatal));
        ret
    }
}


// ── Triggers ─────────────────────────────────────────────────────────────

/// A pointer's sectors added to its bucket's alloc key @a - the stripe, dirty
/// or cached count, as the pointer is - and on insert, the bucket's data type
/// set from it: __mark_pointer().
#[allow(clippy::too_many_arguments)]
fn mark_pointer(t: &TransAttempt<'_, '_>, ca: &Dev, k: BkeySC<'_>, p: &c::extent_ptr_decoded,
                sectors: i64, ptr_data_type: DataType, a: &mut c::AllocV4, insert: bool)
    -> Result<(), BchError>
{
    let (generation, mut bucket_data_type) = (a.generation, a.data_type());
    let dst = if p.has_ec                { &mut a.stripe_sectors }
              else if p.ptr.cached() == 0 { &mut a.dirty_sectors }
              else                         { &mut a.cached_sectors };
    let ret = t.bucket_ref_update(ca, k, &p.ptr, sectors, ptr_data_type, generation, &mut bucket_data_type, dst);
    // as C's, even on error: a mismatch it let through is BCH_DATA_multiple
    a.data_type.set(bucket_data_type);
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
        let _ = fs.err(Errcode::BCH_ERR_trigger_pointer);
    }

    let mut msg = log_msg_ratelimited!(fs);
    prt!(&mut msg, "Error while {} key:\n", if insert { "inserting" } else { "deleting" });
    fs.dev_missing_bkey_msg(k, dev, &mut msg)
}

/// A pointer's bucket - its sectors and its backpointer - in the alloc key,
/// and in gc's copy with gc: bch2_trigger_pointer(). Returns the pointer's
/// sectors on disk, negative going out.
fn trigger_pointer(t: &TransAttempt<'_, '_>, btree: Btree, level: u32, k: BkeySC<'_>,
                   d: &DecodedPtr<'_>, flags: UpdateTriggerFlags) -> Result<i64, BchError> {
    let fs = t.fs();
    let p = &d.p;
    let insert = !flags.contains(UpdateTriggerFlags::OVERWRITE);

    let mut bp = extent_ptr_to_bp(fs, btree, level, k, d);
    let bp_data_type = DataType(bp.v.data_type.into());
    let sectors = if insert { i64::from(bp.v.bucket_len) } else { -i64::from(bp.v.bucket_len) };

    let dev = u32::from(p.ptr.dev());
    if unlikely(dev == c::BCH_SB_MEMBER_INVALID) {
        if flags.contains(UpdateTriggerFlags::TRANSACTIONAL) && p.has_ec {
            if !insert {
                bp.k.set_key_type(BkeyType::deleted);
                bp.k.set_val_bytes(0);
            }
            t.update_buffered(Btree::stripe_backpointers, &bp)?;
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
            return fs.throw(Errcode::BCH_ERR_trigger_pointer);
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
    passes::run_explicit(fs, &mut msg, RecoveryPass::check_allocations,
                         c::RunRecoveryPassFlags::from_bits_retain(0))
}

/// An erasure coded pointer's sectors, in its block of the stripe - the key
/// in the stripes btree, and gc's copy with gc - and in the stripe's
/// replicas accounting: bch2_trigger_stripe_ptr().
fn trigger_stripe_ptr(t: &TransAttempt<'_, '_>, k: BkeySC<'_>, p: &c::extent_ptr_decoded,
                      data_type: DataType, sectors: i64, flags: UpdateTriggerFlags)
    -> Result<(), BchError>
{
    let fs = t.fs();
    let (idx, block) = (p.ec.idx(), p.ec.block());

    if flags.contains(UpdateTriggerFlags::TRANSACTIONAL) {
        // the stripe, queued as its update: bch2_bkey_get_mut_typed()
        let mut iter = BtreeIter::new(t, Btree::stripes, pos(0, idx),
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
        // wrapping to a u16, as C's
        let n = &mut s.v_mut().blockcounts_mut().expect("a checksum type this version knows")[usize::from(block)];
        n.set((i64::from(n.get()) + sectors) as u16);

        let acct = bkey_replicas_acct(fs, s.as_sc().untyped(), data_type);
        return accounting::add_at(t, &acct, &[sectors], false);
    }

    if flags.contains(UpdateTriggerFlags::GC) {
        let Ok(m) = fs.ec.gc_stripe_alloc(idx) else {
            bch_err!(fs, "error allocating memory for gc_stripes, idx {idx}");
            return fs.throw(Errcode::BCH_ERR_ENOMEM_mark_stripe_ptr);
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

            entry_acct(&m.r.bytes, data_type)
        };
        accounting::add_at(t, &acct, &[sectors], true)?;
    }

    Ok(())
}

/// One key's part of the extent trigger, going in or out by @flags: each
/// pointer's bucket and stripe, then the key's accounting - replicas,
/// compression, its snapshot's, and its btree's or inode's: __trigger_extent().
fn trigger_extent_key(t: &TransAttempt<'_, '_>, btree: Btree, level: u32, k: BkeySC<'_, Ptrs>,
                      flags: UpdateTriggerFlags) -> Result<(), BchError> {
    let fs = t.fs();
    let gc = flags.contains(UpdateTriggerFlags::GC);
    let insert = !flags.contains(UpdateTriggerFlags::OVERWRITE);
    let nr: i64 = if insert { 1 } else { -1 };
    let data_type = if k.k.is_btree_ptr() { DataType::btree } else { DataType::user };

    let mut replicas_sectors = 0i64;
    let mut devs = [0u8; c::BCH_BKEY_PTRS_MAX as usize];
    let mut nr_devs = 0;
    let mut nr_required = 1;

    // for each run of pointers of one compression type
    let compression_add = |type_: CompressionType, mut v: accounting::CompressionCounters<i64>| {
        if !insert {
            v = v.map(i64::wrapping_neg);
        }
        accounting::add(t, accounting::Compression { type_: type_.0 as u8 }, v, gc)
    };
    let one_extent = accounting::CompressionCounters { nr_extents: 1, ..Default::default() };
    let mut compression_type = CompressionType::none;
    let mut compression = one_extent;

    for d in k.ptrs_decoded(fs) {
        let p = &d.p;
        let disk_sectors = match trigger_pointer(t, btree, level, k.into(), &d, flags) {
            Ok(s) => s,
            // only a cached pointer is ever stale: it isn't counted
            Err(e) if e.matches(Errcode::BCH_ERR_cached_ptr_stale) => continue,
            Err(e) => return Err(e),
        };

        if p.ptr.cached() != 0 {
            mod_dev_cached_sectors(t, p.ptr.dev().into(), disk_sectors, gc)?;
        } else if !p.has_ec {
            replicas_sectors += disk_sectors;
            devs[nr_devs] = p.ptr.dev();
            nr_devs += 1;
        } else {
            trigger_stripe_ptr(t, k.into(), p, data_type, disk_sectors, flags)?;
            // with an erasure coded pointer, its other dirty pointers aren't
            // needed to mount
            nr_required = 0;
        }

        if compression_type != CompressionType::none && p.crc.compression_type != compression_type {
            compression_add(compression_type, compression)?;
            compression = one_extent;
        }
        compression_type = p.crc.compression_type.get();
        if compression_type != CompressionType::none {
            compression.uncompressed += i64::from(p.crc.uncompressed_size);
            compression.compressed += i64::from(p.crc.compressed_size);
        }
    }

    if nr_devs != 0 {
        accounting::add_at(t, &replicas_acct(data_type, nr_required, &devs[..nr_devs]), &[replicas_sectors], gc)?;
    }

    // an extent counts its own key here, not by trigger_snapshot_nr_keys(),
    // as part of the sectors it adds anyway
    if level == 0 && k.k.p.snapshot != 0 {
        accounting::add(t, accounting::Snapshot { id: k.k.p.snapshot, btree: btree.0 }, accounting::SnapshotCounters {
            keys: nr, key_bytes: nr * k.k.bytes() as i64, sectors: replicas_sectors,
        }, gc)?;
    }

    if compression_type != CompressionType::none {
        compression_add(compression_type, compression)?;
    }

    if level != 0 {
        accounting::add(t, accounting::Btree { id: btree.0 }, accounting::BtreeCounters {
            sectors: replicas_sectors, nodes: nr, interior: if level > 1 { nr } else { 0 },
        }, gc)
    } else {
        accounting::add(t, accounting::Inum { inum: k.k.p.inode }, accounting::InumCounters {
            nr_extents: nr, sectors: nr * i64::from(k.k.size), disk_sectors: replicas_sectors,
        }, gc)
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
       op.level == 0 && !op.new.k.key_type().is_deleted() {
        extent_trigger_set_needs_reconcile(t, op)?;
    }

    // either deleted, or of this trigger's type
    if let Some(old) = op.old.arm::<Ptrs>() {
        trigger_extent_key(t, op.btree, op.level, old, op.flags - UpdateTriggerFlags::INSERT)?;
    }
    if let Some(new) = BkeySC::from(&op.new).arm::<Ptrs>() {
        trigger_extent_key(t, op.btree, op.level, new, op.flags - UpdateTriggerFlags::OVERWRITE)?;
    }

    trigger_extent_reconcile(t, op)
}

pub(crate) static EXTENT: Trigger = Trigger(trigger_extent);

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

        accounting::add(t, accounting::PersistentReserved { nr_replicas },
                        accounting::PersistentReservedCounters { sectors }, gc)?;

        if op.level == 0 && k.k.p.snapshot != 0 {
            accounting::add(t, accounting::Snapshot { id: k.k.p.snapshot, btree: op.btree.0 }, accounting::SnapshotCounters {
                keys: nr, key_bytes: nr * k.k.bytes() as i64, sectors: sectors * i64::from(nr_replicas),
            }, gc)?;
        }
        Ok(())
    })
}

pub(crate) static RESERVATION: Trigger = Trigger(|t, op| trigger_reservation(t, op));

/// Whether @k counts in its snapshot's keys: not a whiteout - iteration skips
/// those, so snapshot deletion never sees them - nor an extent or a
/// reservation, which count themselves.
fn snapshot_key_counted(k: &c::bkey) -> bool {
    k.p.snapshot != 0 &&
        !k.key_type().is_extent_whiteout() &&
        !k.key_type().is::<c::Extent>() &&
        !k.key_type().is::<c::Reservation>()
}

/// Each (snapshot, btree)'s leaf keys and their bytes, so snapshot deletion
/// can check a node is empty before splicing it out: the delta from @op's old
/// key to its new - the transactional dispatch passes a deleted key for the
/// side not running, the gc and mem dispatch both real keys at once. Not in
/// the atomic phase, which only applies deltas already staged.
pub(crate) fn trigger_snapshot_nr_keys(t: &TransAttempt<'_, '_>, op: &TriggerOp<'_>) -> Result<(), BchError> {
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

    // extents and reservations count sectors
    accounting::add(t, accounting::Snapshot { id: op.new.k.p.snapshot, btree: op.btree.0 },
                    accounting::SnapshotCounters { keys: nr, key_bytes: bytes, sectors: 0 },
                    op.flags.contains(UpdateTriggerFlags::GC))
}

// ── Metadata buckets ─────────────────────────────────────────────────────

/// Bucket @b of @ca as @sectors of @data_type in its alloc key - set, not
/// added to, so marking again is a no-op: __bch2_trans_mark_metadata_bucket().
fn trans_mark_metadata_bucket_key(t: &TransAttempt<'_, '_>, ca: &Dev, b: u64,
                                  data_type: DataType, sectors: u32) -> Result<(), BchError> {
    let fs = t.fs();
    let mut iter = BtreeIter::new(t, Btree::alloc, pos(ca.dev_idx.into(), b),
                                  BtreeIterFlags::CACHED | BtreeIterFlags::INTENT);
    let k = iter.peek_slot(t)?.expect("peek_slot() has a key at every position");
    let mut a = t.alloc_to_v4_mut(k)?;

    let cur = a.v().data_type();
    if cur != DataType::free && data_type != DataType::free && cur != data_type {
        let mut msg = LogMsg::new(fs);
        prt!(&mut msg, "bucket {}:{b} gen {} different types of data in same bucket: {}, {}\nwhile marking {}\n",
             ca.dev_idx, a.v().generation, data_type_str(cur),
             data_type_str(data_type), data_type_str(data_type));
        count_fsck_err(fs, id::bucket_metadata_type_mismatch, &mut msg);
        passes::run_explicit(fs, &mut msg, RecoveryPass::check_allocations,
                             c::RunRecoveryPassFlags::from_bits_retain(0))?;
        return fs.throw(Errcode::BCH_ERR_metadata_bucket_inconsistency);
    }

    if cur != data_type || a.v().dirty_sectors != sectors {
        a.v_mut().data_type.set(data_type);
        a.v_mut().dirty_sectors = sectors;
        t.update(&iter, &a, UpdateTriggerFlags::empty())?;
    }
    Ok(())
}

/// trans_mark_metadata_bucket_key(), in gc's copy - which adds: gc counts
/// each bucket's metadata once: bch2_mark_metadata_bucket().
fn mark_metadata_bucket_gc(t: &TransAttempt<'_, '_>, ca: &Dev, b: u64, data_type: DataType,
                           sectors: u32, flags: UpdateTriggerFlags) -> Result<(), BchError> {
    let fs = t.fs();
    let Some(g) = ca.gc_bucket(b) else {
        fs_inconsistent(fs, message!("reference to invalid bucket on device {} when marking metadata type {}",
                                     ca.dev_idx, data_type_str(data_type)));
        return fs.throw(Errcode::BCH_ERR_metadata_bucket_inconsistency);
    };

    t.gc_bucket_mod(ca, g, flags, |a| {
        let cur = a.data_type();
        if cur != DataType::free && cur != data_type {
            fs_inconsistent(fs, message!("different types of data in same bucket: {}, {}",
                                         data_type_str(cur), data_type_str(data_type)));
            return fs.throw(Errcode::BCH_ERR_metadata_bucket_inconsistency);
        }
        if u64::from(a.dirty_sectors) + u64::from(sectors) > u64::from(ca.mi.get().bucket_size) {
            fs_inconsistent(fs, message!(
                "bucket {}:{b} gen {} data type {} sector count overflow: {} + {sectors} > bucket size",
                ca.dev_idx, a.generation, data_type_str(if cur != DataType::free { cur } else { data_type }),
                a.dirty_sectors));
            return fs.throw(Errcode::BCH_ERR_metadata_bucket_inconsistency);
        }

        a.data_type.set(data_type);
        a.dirty_sectors += sectors;
        Ok(())
    })
}

/// Bucket @b of @ca holds @sectors of metadata - superblock, journal, or free
/// to unmark it: in its alloc key, committed, or in gc's copy:
/// bch2_trans_mark_metadata_bucket(). The gc side runs in an attempt of its
/// own; it's only reached on a transaction made for it.
pub fn trans_mark_metadata_bucket(trans: &TransGuard<'_>, ca: &Dev, b: u64, data_type: DataType,
                                  sectors: u32, flags: UpdateTriggerFlags) -> Result<(), BchError> {
    assert!(matches!(data_type, DataType::free | DataType::sb | DataType::journal),
            "marking {} as metadata", data_type_str(data_type));

    // a backup superblock can be past the end of the usable space
    if b >= ca.mi.get().nbuckets {
        return Ok(());
    }

    if flags.contains(UpdateTriggerFlags::GC) {
        lockrestart_do(trans, |t| mark_metadata_bucket_gc(t, ca, b, data_type, sectors, flags))
    } else if flags.contains(UpdateTriggerFlags::TRANSACTIONAL) {
        commit_do(trans, None, CommitOpts::new().watermark(Watermark::btree),
                  |t| trans_mark_metadata_bucket_key(t, ca, b, data_type, sectors))
    } else {
        panic!("marking metadata, neither gc nor transactional");
    }
}

/// @range's sectors on @ca, metadata of @data_type, into @bucket - a bucket
/// and its count so far - marking the bucket when the range moves past it:
/// bch2_trans_mark_metadata_sectors().
fn trans_mark_metadata_sectors(trans: &TransGuard<'_>, ca: &Dev, range: core::ops::Range<u64>,
                               data_type: DataType, bucket: &mut (u64, u32),
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
/// buckets.
fn trans_mark_dev_sb_in(trans: &TransGuard<'_>, ca: &Dev, flags: UpdateTriggerFlags)
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
            trans_mark_metadata_sectors(trans, ca, 0..sb_sector, DataType::sb, &mut bucket, flags)?;
        }
        trans_mark_metadata_sectors(trans, ca, offset..offset + sb_sectors, DataType::sb, &mut bucket, flags)?;
    }
    if bucket.1 != 0 {
        trans_mark_metadata_bucket(trans, ca, bucket.0, DataType::sb, bucket.1, flags)?;
    }

    /* each under the journal lock: marking sleeps */
    let owned = |i| ca.with_journal_ring(|r| r.buckets().get(i).map(|&b| r.owns(b).then_some(b)));
    for b in (0..).map_while(owned).flatten() {
        trans_mark_metadata_bucket(trans, ca, b, DataType::journal, ca.mi.get().bucket_size.into(), flags)?;
    }
    Ok(())
}

impl Fs {
    /// Device @ca's superblock and journal buckets marked as theirs, in the
    /// alloc btree - or gc's copy, with gc.
    pub fn trans_mark_dev_sb(&self, ca: &Dev, flags: UpdateTriggerFlags) -> Result<(), BchError> {
        let trans = btree_trans!(self);
        bch_err_fn!(self, trans_mark_dev_sb_in(&trans, ca, flags))
    }

    /// trans_mark_dev_sb() of every online device - each initialized first,
    /// if it wasn't: bch2_trans_mark_dev_sbs_flags().
    pub fn trans_mark_dev_sbs(&self, flags: UpdateTriggerFlags) -> Result<(), BchError> {
        for ca in self.online_members() {
            // marked again, even if it was: harmless if not needed, and a
            // superblock or journal bucket allocated twice would be painful
            self.dev_add_initialize(&ca)?;
            self.trans_mark_dev_sb(&ca, flags)?;
        }
        Ok(())
    }
}

crate::recovery_pass!(trans_mark_dev_sbs => |fs: &Fs| fs.trans_mark_dev_sbs(UpdateTriggerFlags::TRANSACTIONAL));

// ── Startup and shutdown ─────────────────────────────────────────────────

impl Fs {
    /// Every device's buckets_nouse, a zeroed bit per bucket: for migrate, to
    /// mark the space it can't use. bch2_buckets_nouse_alloc().
    pub fn buckets_nouse_alloc(&self) -> Result<(), BchError> {
        let state = self.state_lock_write();
        for ca in self.members() {
            let nouse = state.writer_of(&ca.buckets_nouse);
            assert!(nouse.with(|w| w.is_empty()), "buckets_nouse allocated twice");
            if nouse.rebuild(bits_to_longs(ca.mi.get().nbuckets as usize), |_| ()).is_err() {
                return self.throw(Errcode::BCH_ERR_ENOMEM_buckets_nouse);
            }
        }
        Ok(())
    }
}

impl Dev {
    /// Its buckets' generations at @nbuckets - as many of the old copied as
    /// both have - and its bucket bitmaps with them; nothing changed on
    /// failure.
    pub fn buckets_resize(&self, state: &StateLockWrite<'_>, nbuckets: u64) -> Result<(), BchError> {
        self.buckets_resize_writer(state.writer_of(&self.bucket_gens), nbuckets)
    }

    /// buckets_resize()'s, by @gens, bucket_gens' writer: under state_lock,
    /// or through the only reference to the device.
    fn buckets_resize_writer(&self, gens: &RcuArrayWriter<AtomicU8, c::BucketGensHead>, nbuckets: u64)
        -> Result<(), BchError>
    {
        let fs = self.fs();
        if gens.read_head(&read_lock()).is_some() && self.buckets_nouse.with(|w| !w.is_empty()) {
            return fs.throw(Errcode::BCH_ERR_no_resize_with_buckets_nouse);
        }

        let head = c::BucketGensHead {
            first_bucket:         self.mi.get().first_bucket,
            nbuckets_minus_first: (nbuckets - u64::from(self.mi.get().first_bucket)) as usize,
        };
        gens.rebuild_head(nbuckets as usize, head, |old, gens| {
            for (new, old) in gens.iter_mut().zip(old.map_or(&[][..], |(_, old)| old)) {
                *new.get_mut() = old.load(Ordering::Relaxed);
            }
            self.bucket_backpointer_mismatch.resize(self, nbuckets)?;
            self.bucket_backpointer_empty.resize(self, nbuckets)
        }).or_else(|_| fs.throw(Errcode::BCH_ERR_ENOMEM_bucket_gens))?
    }

    /// Its usage counters and bucket generations, as it's set up:
    /// bch2_dev_buckets_alloc().
    pub fn buckets_alloc(&mut self) -> Result<(), BchError> {
        match PerCpu::alloc() {
            Ok(u)  => self.usage = u,
            Err(_) => return self.fs().throw(Errcode::BCH_ERR_ENOMEM_usage_init),
        }
        // a new device: no old gens to keep, no backpointer bitmaps to resize
        let mi = self.mi.get();
        let head = c::BucketGensHead {
            first_bucket:         mi.first_bucket,
            nbuckets_minus_first: (mi.nbuckets - u64::from(mi.first_bucket)) as usize,
        };
        let gens = self.bucket_gens.published_writer_mut();
        debug_assert!(gens.read_head(&read_lock()).is_none(), "bucket gens allocated twice");
        if gens.rebuild_head(mi.nbuckets as usize, head, |_, _| Ok::<(), Infallible>(())).is_err() {
            return self.fs().throw(Errcode::BCH_ERR_ENOMEM_bucket_gens);
        }
        Ok(())
    }

    /// What buckets_alloc() and buckets_nouse_alloc() allocated, as it's
    /// freed: bch2_dev_buckets_free().
    pub fn buckets_free(&mut self) {
        unsafe {
            self.buckets_nouse.free();
            self.bucket_gens.free();
        }
        self.usage.free();
    }
}

// ── Usage ────────────────────────────────────────────────────────────────

impl Dev {
    /// The buckets held back from allocations at @watermark - for those at the
    /// watermarks under it: bch2_dev_buckets_reserved().
    pub fn buckets_reserved(&self, watermark: c::Watermark) -> u64 {
        use c::Watermark as w;
        let (some, btree) = (self.mi.get().nbuckets >> 6, u64::from(self.nr_btree_reserve));

        match watermark {
            w::stripe => 2 * some + 2 * btree,
            w::normal => some + 2 * btree,
            w::copygc => 2 * btree,
            w::btree  => btree,
            w::btree_copygc |
            w::reclaim |
            w::interior_updates => 0,
        }
    }

    /// Its free buckets in @usage, less the open ones and @watermark's
    /// reserve, at least 0: __dev_buckets_free(). nr_open_buckets is the
    /// allocator's, read unlocked, as C does.
    pub fn free_buckets_at(&self, usage: &c::bch_dev_usage, watermark: c::Watermark) -> u64 {
        let open = self.nr_open_buckets.get();
        let free = usage.buckets[DataType::free];
        // as C's: u64s, then max_t(s64, 0, ..)
        (free.wrapping_sub(open.into()).wrapping_sub(self.buckets_reserved(watermark)) as i64).max(0) as u64
    }

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

    /// @usage, of this device, as a table.
    pub fn usage_to_text(&self, out: &mut Printbuf, usage: &c::bch_dev_usage_full) {
        if out.nr_tabstops() < 5 {
            out.tabstops_reset();
            for spaces in [12, 16, 16, 16, 16] {
                out.tabstop_push(spaces);
            }
        }

        write!(out, "\tbuckets\rsectors\rfragmented\r\n");
        for (i, u) in usage.d.iter().enumerate() {
            DataType(u32::from(i as u8)).to_text(out);
            write!(out, "\t{}\r{}\r{}\r\n", u.buckets, u.sectors, u.fragmented);
        }
        write!(out, "capacity\t{}\r\n", self.mi.get().nbuckets);
    }
}

/// The bucket @v encodes - device in the top 16 bits, bucket in the low 48 -
/// as the lru btree's targets and need_discard's positions do:
/// u64_to_bucket().
pub fn u64_to_bucket(v: u64) -> Bpos {
    pos(v >> 48, v & ((1 << 48) - 1))
}

/// u64_to_bucket()'s inverse: bucket_to_u64().
pub fn bucket_to_u64(bucket: Bpos) -> u64 {
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
impl Dev {
    /// Where bucket @b's generation is, for as long as @rcu's section - None
    /// if it isn't one of the device's: bucket_gen().
    pub fn bucket_gen_ref<'a>(&'a self, b: u64, rcu: &'a ReadGuard) -> Option<&'a AtomicU8> {
        let (h, gens) = self.bucket_gens.read_head(rcu)?;
        (b.wrapping_sub(h.first_bucket.into()) < h.nbuckets_minus_first as u64)
            .then(|| gens.get(b as usize))
            .flatten()
    }

    /// Bucket @b's generation: bucket_gen_get_rcu().
    pub fn bucket_gen(&self, b: u64, rcu: &ReadGuard) -> Option<u8> {
        self.bucket_gen_ref(b, rcu).map(|g| g.load(Ordering::Relaxed))
    }

    /// How many generations @ptr's bucket has moved on from it - None for a
    /// bucket the device doesn't have: dev_ptr_stale_rcu(), which is -1 there.
    pub fn ptr_stale(&self, ptr: &c::bch_extent_ptr, rcu: &ReadGuard) -> Option<u8> {
        Some(gen_after(self.bucket_gen(self.ptr_bucket(ptr), rcu)?, ptr.generation()))
    }

    /// Whether @ptr's bucket has moved on from it, or isn't the device's:
    /// dev_ptr_stale().
    pub fn dev_ptr_stale(&self, ptr: &c::bch_extent_ptr, rcu: &ReadGuard) -> bool {
        self.ptr_stale(ptr, rcu) != Some(0)
    }
}
