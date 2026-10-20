// SPDX-License-Identifier: GPL-2.0

//! The lru btree: per-device read and fragmentation lrus, and the stripe lru.
//!
//! An entry is a set bit at an LruPos: which lru, a time, and the entry's
//! target - a bucket (bucket_to_u64()) for the read and fragmentation lrus, a
//! stripe index for the stripe lru. It's encoded into a bpos as inode = id <<
//! LRU_TIME_BITS | time, offset = target, so the bpos order is (id, time,
//! target): an lru is one contiguous range, oldest first. LruPos is that
//! position, its own type rather than a bpos, converted only at the btree.
//!
//! What an id means - LruId - is a function of the id: ranges of it are the
//! per-device lrus, the device a part of the id. The raw id is what's stored
//! and ordered by, never the decoded LruId.
//!
//! lru.h's inlines are the same encoding, for C's callers.

use crate::alloc::background::alloc_to_v4;
use crate::alloc::buckets::u64_to_bucket;
use crate::btree::bkey::prelude::*;
use crate::btree::bkey::{pos, spos, BkeySC, POS_MIN, SPOS_MAX};
use crate::btree::iter::{lockrestart_do, BtreeIter, BtreeIterFlags, BtreeTrans, CommitFlags,
                         TransAttempt, UpdateTriggerFlags};
use crate::btree::write_buffer::WbMaybeFlush;
use crate::c;
use crate::errcode::{ret_to_c, ret_to_result_void, BchError};
use crate::fs::Fs;
use crate::init::error::{id, BkeyValidate};
use crate::init::progress::Progress;
use crate::util::ffi::Opaque;
use crate::util::rcu::read_lock;
use crate::util::Printbuf;
use crate::{bkey_fsck_err_on, fsck_err};
use core::ffi::c_int;
use core::ops::RangeInclusive;
use cstruct_macros::c_export;

/// What an lru id says: which lru, and for a per-device one, the device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LruId {
    Read { dev: u32 },
    Fragmentation { dev: u32 },
    /// Before per_dev_fragmentation_lru: one lru for every device's buckets
    FragmentationOld,
    Stripes,
    /// None of ours: check_lrus() deletes its entries
    Unknown(u16),
}

impl From<u16> for LruId {
    fn from(id: u16) -> Self {
        let id32 = u32::from(id);

        if id32 < c::BCH_LRU_READ_MAX {
            Self::Read { dev: id32 }
        } else if (c::BCH_LRU_BUCKET_FRAGMENTATION_START..c::BCH_LRU_BUCKET_FRAGMENTATION_END).contains(&id32) {
            Self::Fragmentation { dev: id32 - c::BCH_LRU_BUCKET_FRAGMENTATION_START }
        } else if id32 == c::BCH_LRU_BUCKET_FRAGMENTATION_OLD {
            Self::FragmentationOld
        } else if id32 == c::BCH_LRU_STRIPE_FRAGMENTATION {
            Self::Stripes
        } else {
            Self::Unknown(id)
        }
    }
}

impl LruId {
    /// The inverse of From<u16>: a device past its lru's id range is a bug.
    pub fn id(self) -> u16 {
        let id = match self {
            Self::Read { dev }          => dev,
            Self::Fragmentation { dev } => c::BCH_LRU_BUCKET_FRAGMENTATION_START + dev,
            Self::FragmentationOld      => c::BCH_LRU_BUCKET_FRAGMENTATION_OLD,
            Self::Stripes               => c::BCH_LRU_STRIPE_FRAGMENTATION,
            Self::Unknown(id)           => u32::from(id),
        } as u16;

        debug_assert_eq!(Self::from(id), self);
        id
    }

    /// Its type, an unknown id's being read, as lru_type() has it: its
    /// entries' targets are read as buckets.
    pub fn lru_type(self) -> c::bch_lru_type {
        use c::bch_lru_type as T;

        match self {
            Self::Read { .. } | Self::Unknown(_)           => T::BCH_LRU_read,
            Self::Fragmentation { .. } | Self::FragmentationOld => T::BCH_LRU_fragmentation,
            Self::Stripes                                  => T::BCH_LRU_stripes,
        }
    }
}

/// A position in the lru btree. Ordered as the bpos it's encoded in -
/// (id, time, target) - so an lru is one contiguous range, oldest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct LruPos {
    pub id:     u16,
    /// LRU_TIME_BITS wide
    pub time:   u64,
    pub target: u64,
}

impl LruPos {
    pub fn new(id: u16, time: u64, target: u64) -> Self {
        debug_assert!(time <= c::LRU_TIME_MAX);
        Self { id, time, target }
    }

    /// Any position in the lru btree decodes: an id is all of inode's top
    /// bits.
    pub fn from_bpos(p: c::bpos) -> Self {
        Self {
            id:     (p.inode >> c::LRU_TIME_BITS) as u16,
            time:   p.inode & c::LRU_TIME_MAX,
            target: p.offset,
        }
    }

    pub fn to_bpos(self) -> c::bpos {
        pos(u64::from(self.id) << c::LRU_TIME_BITS | self.time, self.target)
    }

    /// Every position of lru @id: lru_start()..=lru_end().
    pub fn range(id: LruId) -> RangeInclusive<Self> {
        Self::new(id.id(), 0, 0)..=Self::new(id.id(), c::LRU_TIME_MAX, u64::MAX)
    }

    pub fn lru_id(self) -> LruId {
        LruId::from(self.id)
    }

    /// The key the entry is for: lru_pos_to_bp().
    pub fn backing(self) -> c::bbpos {
        match self.lru_id().lru_type() {
            c::bch_lru_type::BCH_LRU_stripes => c::bbpos {
                btree: c::btree_id::stripes,
                pos:   pos(0, self.target),
            },
            _ => c::bbpos {
                btree: c::btree_id::alloc,
                pos:   u64_to_bucket(self.target),
            },
        }
    }
}

/// The bch_lru_type names, by type: bch2_lru_types[].
macro_rules! lru_type_strs {
    ($(($n:ident)),* $(,)?) => { [$(stringify!($n)),*] };
}
const LRU_TYPE_STRS: [&str; 3] = c::BCH_LRU_TYPES!(lru_type_strs);

fn lru_type_str(t: c::bch_lru_type) -> &'static str {
    LRU_TYPE_STRS.get(t.0 as usize).copied().unwrap_or("(unknown)")
}

// ── The key type ─────────────────────────────────────────────────────────
//
// An lru entry is a KEY_TYPE_set; KEY_TYPE_lru, with an idx, is obsolete.

fn lru_validate(v: &BkeyValidate<'_, '_>) -> Result<(), BchError> {
    bkey_fsck_err_on!(v, LruPos::from_bpos(v.k.k.p).time == 0, id::lru_entry_at_time_0,
                      "lru entry at time=0")
}

#[c_export]
pub fn bch2_lru_validate(c: &Opaque<c::bch_fs>, k: BkeySC<'_>, from: &c::bkey_validate_context) -> c_int {
    ret_to_c(lru_validate(&BkeyValidate { fs: Fs::from_c(c), k, from }))
}

#[cold]
#[c_export]
pub fn bch2_lru_to_text(out: &mut Printbuf, _c: &Opaque<c::bch_fs>, k: BkeySC<'_>) {
    write!(out, "idx {}", k.arm::<c::Lru>().expect("an lru key").v().idx.get());
}

// ── Setting entries ──────────────────────────────────────────────────────

/// Set or clear @pos's bit - there's none at time 0: nothing to do.
pub(crate) fn lru_mod(t: &TransAttempt<'_, '_>, pos: LruPos, set: bool) -> Result<(), BchError> {
    if pos.time == 0 {
        return Ok(());
    }
    t.bit_mod_buffered(c::btree_id::lru, pos.to_bpos(), set)
}

/// # Safety
/// @trans is in an attempt.
#[c_export]
pub unsafe extern "C" fn bch2_lru_set<'t>(trans: &'t BtreeTrans<'t>, lru_id: u16,
                                          dev_bucket: u64, time: u64) -> c_int {
    let t = unsafe { trans.attempt_in_progress() };
    ret_to_c(lru_mod(&t, LruPos::new(lru_id, time, dev_bucket), true))
}

/// Move @dev_bucket's entry in lru @lru_id from @old_time to @new_time:
/// bch2_lru_change()'s slow path, with old_time != new_time.
///
/// # Safety
/// @trans is in an attempt.
#[c_export]
pub unsafe extern "C" fn __bch2_lru_change<'t>(trans: &'t BtreeTrans<'t>, lru_id: u16,
                                               dev_bucket: u64, old_time: u64, new_time: u64) -> c_int {
    let t = unsafe { trans.attempt_in_progress() };
    let pos = |time| LruPos::new(lru_id, time, dev_bucket);
    ret_to_c(lru_mod(&t, pos(old_time), false).and_then(|()| lru_mod(&t, pos(new_time), true)))
}

// ── Removing a device ────────────────────────────────────────────────────

/// Every entry of device @dev's buckets, wherever they are - the old
/// fs-wide fragmentation lru's are mixed in with every device's.
fn dev_remove_lrus_scan(fs: &Fs, dev: u32) -> Result<(), BchError> {
    let trans = crate::btree_trans!(fs);
    let mut iter = BtreeIter::new(&trans, c::btree_id::lru, POS_MIN, BtreeIterFlags::PREFETCH);

    iter.for_each_commit(&trans, None, CommitFlags::empty(), |t, iter, k| {
        let bp = LruPos::from_bpos(k.k.p).backing();
        if bp.btree == c::btree_id::alloc && bp.pos.inode == u64::from(dev) {
            t.delete_at(iter, UpdateTriggerFlags::empty())?;
        }
        Ok(())
    })
}

/// Lru @id's entries, all of it.
fn dev_remove_lru_range(fs: &Fs, id: LruId) -> Result<(), BchError> {
    let range = LruPos::range(id);
    // In a transaction of its own, not the trans's delete_range(), whose
    // transaction_restart_nested this has no attempt to begin again for. The
    // end is inclusive: lru_end(), not the next lru's start.
    ret_to_result_void(unsafe {
        c::bch2_btree_delete_range(fs.as_ptr(), c::btree_id::lru, range.start().to_bpos(),
                                   range.end().to_bpos(), c::btree_iter_update_trigger_flags::empty())
    })
}

/// The entries of device @ca's buckets, as it's removed: its own lrus, if it
/// has them - a device past the per-device lrus' ids has its entries found
/// by scanning - and before per_dev_fragmentation_lru, the fs-wide
/// fragmentation lru's, found by scanning.
fn dev_remove_lrus(fs: &Fs, ca: &c::bch_dev) -> Result<(), BchError> {
    crate::btree_trans!(fs).write_buffer_flush_sync()?;

    let dev = u32::from(ca.dev_idx);
    if dev < c::BCH_LRU_READ_MAX {
        dev_remove_lru_range(fs, LruId::Read { dev })?;
        dev_remove_lru_range(fs, LruId::Fragmentation { dev })?;
    } else {
        dev_remove_lrus_scan(fs, dev)?;
    }

    if !per_dev_fragmentation_lru(fs) {
        dev_remove_lrus_scan(fs, dev)?;
    }
    Ok(())
}

/// Whether the upgrade to the per-device fragmentation lrus is done: before
/// it, entries can be in the old fs-wide one.
pub(crate) fn per_dev_fragmentation_lru(fs: &Fs) -> bool {
    fs.version_upgrade_complete().0 >=
        c::bcachefs_metadata_version::bcachefs_metadata_version_per_dev_fragmentation_lru.0
}

#[c_export]
pub fn bch2_dev_remove_lrus(c: &Opaque<c::bch_fs>, ca: &c::bch_dev) -> c_int {
    let fs = Fs::from_c(c);
    ret_to_c(crate::bch_err_fn!(fs, dev_remove_lrus(fs, ca)))
}

// ── fsck ─────────────────────────────────────────────────────────────────

/// That the entry at @pos, which @referring_k - an alloc or stripe key - says
/// it has, is there: set it if not, as fsck says. The write buffer is flushed
/// first, in case it's there, once per referring key - @last_flushed's.
pub(crate) fn lru_check_set(
    t:            &TransAttempt<'_, '_>,
    pos:          LruPos,
    referring_k:  BkeySC<'_>,
    last_flushed: *mut c::wb_maybe_flush,
) -> Result<(), BchError> {
    let fs = t.fs();
    let mut iter = BtreeIter::new(t, c::btree_id::lru, pos.to_bpos(), BtreeIterFlags::empty());
    let lru_k = iter.peek_slot(t)?.expect("a slot always has a key");

    if lru_k.k.type_ != c::bch_bkey_type::KEY_TYPE_set.0 as u8 {
        // SAFETY: the caller's
        ret_to_result_void(unsafe {
            c::bch2_btree_write_buffer_maybe_flush(t.raw(), referring_k.into(), last_flushed)
        })?;

        if fsck_err!(t, id::alloc_key_to_missing_lru_entry, "missing {} lru entry at pos {}\n{}",
                     lru_type_str(pos.lru_id().lru_type()), iter.pos(), referring_k.to_text(fs))? {
            lru_mod(t, pos, true)?;
        }
    }
    Ok(())
}

/// # Safety
/// @trans is in an attempt; @last_flushed is the pass's.
#[c_export]
pub unsafe extern "C" fn bch2_lru_check_set<'t>(
    trans:        &'t BtreeTrans<'t>,
    lru_id:       u16,
    dev_bucket:   u64,
    time:         u64,
    referring_k:  BkeySC<'_>,
    last_flushed: *mut c::wb_maybe_flush,
) -> c_int {
    let t = unsafe { trans.attempt_in_progress() };
    ret_to_c(lru_check_set(&t, LruPos::new(lru_id, time, dev_bucket), referring_k, last_flushed))
}

/// The time @k says its entry in an lru of type @ty is at - 0, none:
/// bkey_lru_type_idx().
fn expected_time(fs: &Fs, ty: c::bch_lru_type, k: BkeySC<'_>) -> u64 {
    use c::bch_lru_type as T;

    match ty {
        T::BCH_LRU_read => alloc_to_v4(k).lru_idx_read(),
        T::BCH_LRU_fragmentation => {
            let a = alloc_to_v4(k);
            let rcu = read_lock();
            fs.dev_rcu(k.k.p.inode as u32, &rcu).map_or(0, |ca| a.lru_idx_fragmentation(ca))
        }
        T::BCH_LRU_stripes => k.arm::<c::Stripe>().map_or(0, |s| s.v().lru_pos(s.val_bytes())),
        t => unreachable!("lru type {}", t.0),
    }
}

/// That the entry at @lru_k is in the lru its target's key says, at the time
/// it says - deleting it if not, as fsck says.
fn check_lru_key(
    t:            &TransAttempt<'_, '_>,
    lru_iter:     &BtreeIter<'_>,
    lru_k:        BkeySC<'_>,
    last_flushed: &mut WbMaybeFlush,
) -> Result<(), BchError> {
    let fs = t.fs();
    let pos = LruPos::from_bpos(lru_k.k.p);
    let lru = pos.lru_id();

    // per_dev_fragmentation_lru upgrade: the old fs-wide fragmentation lru is
    // deleted, its entries not checked - check_alloc_to_lru_refs recreates
    // them in the per-device lrus, from the alloc btree
    if lru == LruId::FragmentationOld && !per_dev_fragmentation_lru(fs) {
        return t.bit_mod_buffered(c::btree_id::lru, lru_iter.pos(), false);
    }

    let bp = pos.backing();
    let mut iter = BtreeIter::new(t, bp.btree, bp.pos, BtreeIterFlags::empty());
    let k = iter.peek_slot(t)?.expect("a slot always has a key");

    let ty = lru.lru_type();
    let time = expected_time(fs, ty, k);

    // The read and bucket fragmentation lrus are per-device: the lru its
    // bucket's device says, by the raw id - a corrupt target's device needn't
    // be one an lru id can name. That's also what deletes stragglers in the old
    // fs-wide fragmentation lru after the upgrade.
    let bucket_dev = u64_to_bucket(pos.target).inode as u16;
    let want_id = match ty {
        c::bch_lru_type::BCH_LRU_read          => bucket_dev,
        c::bch_lru_type::BCH_LRU_fragmentation => (c::BCH_LRU_BUCKET_FRAGMENTATION_START as u16)
                                                    .wrapping_add(bucket_dev),
        _                                      => pos.id,
    };

    if pos.id != want_id || pos.time != time {
        last_flushed.maybe_flush(t, lru_k)?;

        if fsck_err!(t, id::lru_entry_bad,
                     "incorrect lru entry: lru {}, expected id {} time {}\n{}\nfor {}",
                     lru_type_str(ty), want_id, time, lru_k.to_text(fs), k.to_text(fs))? {
            return t.bit_mod_buffered(c::btree_id::lru, lru_iter.pos(), false);
        }
    }
    Ok(())
}

/// Pin lru @lru's backing keys in the btree node cache: a per-device lru's
/// are its device's slice of the alloc btree, the stripe lru's the stripes
/// btree. The others' entries - the old fs-wide fragmentation lru, ids nothing
/// uses - are scattered or bogus, stragglers to be deleted: no pin.
fn pin_backing(trans: &BtreeTrans<'_>, lru: LruId) -> Result<(), BchError> {
    let (btree, start, end) = match lru {
        LruId::Read { dev } | LruId::Fragmentation { dev } =>
            (c::btree_id::alloc, pos(dev.into(), 0), spos(dev.into(), u64::MAX, u32::MAX)),
        LruId::Stripes =>
            (c::btree_id::stripes, POS_MIN, SPOS_MAX),
        LruId::FragmentationOld | LruId::Unknown(_) => {
            // SAFETY: the btree cache's
            unsafe { c::bch2_btree_cache_unpin(trans.fs().as_ptr()) };
            return Ok(());
        }
    };
    let mask = 1 << btree.0;
    let end = c::bbpos { btree, pos: end };
    let mut pinned_end = end;
    // SAFETY: @trans is ours
    ret_to_result_void(unsafe {
        c::bch2_btree_cache_pin(trans.raw(), mask, mask, c::bbpos { btree, pos: start }, end,
                                &mut pinned_end)
    })?;

    if pinned_end != end {
        let fs = trans.fs();
        crate::bch_warn_ratelimited!(fs,
            "memory budget ({}% of ram) exhausted pinning btree {btree} at {}, requested range end {}; \
             continuing, lookups past this point will be uncached",
            fs.opts.fsck_memory_usage_percent, pinned_end.pos, end.pos);
    }
    Ok(())
}

/// Every lru entry, an lru at a time, its backing keys pinned in the btree
/// node cache: the lookups are random-order, so on a cold cache that turns a
/// read per key into cache hits.
fn check_lrus_pinned(fs: &Fs, trans: &BtreeTrans<'_>) -> Result<(), BchError> {
    let mut last_flushed = WbMaybeFlush::new()?;
    let progress = Progress::recovery(fs, c"bch2_check_lrus", &[c::btree_id::lru], &[]);
    let mut next = POS_MIN;

    loop {
        // The next lru with entries
        let Some(id) = lockrestart_do(trans, |t| {
            let mut iter = BtreeIter::new(t, c::btree_id::lru, next, BtreeIterFlags::empty());
            Ok(iter.peek(t)?.map(|k| LruPos::from_bpos(k.k.p).id))
        })? else { return Ok(()) };

        let lru = LruId::from(id);
        pin_backing(trans, lru)?;

        let range = LruPos::range(lru);
        let mut iter = BtreeIter::new(trans, c::btree_id::lru, range.start().to_bpos(),
                                      BtreeIterFlags::PREFETCH);
        iter.for_each_max_commit(trans, range.end().to_bpos(), BtreeIterFlags::PREFETCH, None,
                                 CommitFlags::NO_ENOSPC, |t, iter, k| {
            progress.update(t, iter)?;
            last_flushed.inc();
            check_lru_key(t, iter, k, &mut last_flushed)
        })?;

        let Some(id) = id.checked_add(1) else { return Ok(()) };
        next = LruPos::new(id, 0, 0).to_bpos();
    }
}

fn check_lrus(fs: &Fs) -> Result<(), BchError> {
    let trans = crate::btree_trans!(fs);
    let ret = check_lrus_pinned(fs, &trans);
    // SAFETY: the btree cache's
    unsafe { c::bch2_btree_cache_unpin(fs.as_ptr()) };
    ret
}

crate::recovery_pass!(bch2_check_lrus => check_lrus);
