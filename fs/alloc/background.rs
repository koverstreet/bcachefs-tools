// SPDX-License-Identifier: GPL-2.0

//! Alloc keys - background.c's, as it converts. So far, what fsck needs to
//! know where a bucket belongs in the lrus, and in need_discard, freespace and
//! bucket_gens.
//!
//! These are background.h's inlines, for Rust, while C still has its own: the
//! alloc trigger writes the entries by C's, fsck checks them by these. The two
//! must agree exactly - to the C integer types, wrapping and all - or fsck
//! calls good entries bad.

use crate::accounting::{data_type, data_type_from_u8, data_type_is_empty};
use crate::alloc::buckets::{bucket_to_u64, u64_to_bucket};
use crate::btree::bkey::prelude::*;
use crate::btree::bkey::{pos, BkeyI, BkeySC};
use crate::btree::iter::{TransAttempt, TransBkey};
use crate::c;
use crate::errcode::{errptr_to_result, BchError};
use crate::fs::Fs;
use crate::util::kernel::READ;
use core::sync::atomic::{AtomicI64, Ordering};

/// Alloc key @k, of whatever version, as a current one - one that isn't an
/// alloc key, the slot of a bucket without one, as a zeroed one:
/// bch2_alloc_to_v4().
pub fn alloc_to_v4(k: BkeySC<'_>) -> c::AllocV4 {
    let mut a = c::AllocV4::default();
    // SAFETY: @k is a key, @a an alloc_v4 to fill
    unsafe { c::__bch2_alloc_to_v4(k.into(), &mut a) };
    a
}

impl Fs {
    /// The io clock for @rw, as a bucket's io_time holds it - never 0, which
    /// is none: bch2_current_io_time().
    pub fn current_io_time(&self, rw: usize) -> u64 {
        // SAFETY: an atomic64_t, which C updates: atomic64_read()
        let now = unsafe { &*(&raw const self.io_clock[rw].now).cast::<AtomicI64>() };
        (now.load(Ordering::Relaxed) as u64 & c::LRU_TIME_MAX).max(1)
    }
}

impl<'a, 't> TransAttempt<'a, 't> {
    /// alloc_to_v4(), as a key in transaction memory to edit and update with
    /// - not queued as an update: bch2_alloc_to_v4_mut().
    pub fn alloc_to_v4_mut(&self, k: BkeySC<'_>) -> Result<TransBkey<'a, 't, c::AllocV4>, BchError> {
        // SAFETY: @k is a key
        let a = errptr_to_result(unsafe { c::bch2_alloc_to_v4_mut(self.raw(), k.into()) })?;

        // Grown to all of an alloc_v4, or as long as @k if it's longer:
        let u64s = u32::from(unsafe { (*a).k.u64s })
            .max((size_of::<BkeyI<c::AllocV4>>() / size_of::<u64>()) as u32);
        // SAFETY: an alloc_v4 key in our memory, with @u64s u64s of room
        let a = unsafe { TransBkey::from_raw_room(self, a.cast(), u64s) };
        // SAFETY: all of an alloc_v4
        Ok(unsafe { a.whole_unchecked() })
    }
}

impl c::AllocV4 {
    fn data_type(&self) -> data_type {
        data_type_from_u8(self.data_type)
    }

    /// bch2_bucket_sectors_dirty(): u32s added as C's, wrapping.
    fn sectors_dirty(&self) -> i64 {
        i64::from(self.stripe_sectors.wrapping_add(self.dirty_sectors))
    }

    /// bch2_bucket_sectors().
    fn sectors(&self) -> i64 {
        if self.data_type() == data_type::cached {
            i64::from(self.cached_sectors)
        } else {
            self.sectors_dirty()
        }
    }

    /// The sectors of @ca's bucket that aren't in use, a full bucket of them if
    /// it's empty but not free: bch2_bucket_sectors_fragmented(), which takes
    /// the sectors as an int.
    fn sectors_fragmented(&self, ca: &c::bch_dev) -> i64 {
        let d = self.sectors() as i32;
        let bucket_size = i32::from(ca.mi.bucket_size);

        i64::from(if d != 0 {
            (bucket_size - d).max(0)
        } else if !data_type_is_empty(self.data_type()) {
            bucket_size
        } else {
            0
        })
    }

    /// Its time in its device's read lru, 0 for none - only cached data is
    /// in it: alloc_lru_idx_read().
    pub fn lru_idx_read(&self) -> u64 {
        if self.data_type() == data_type::cached {
            self.io_time[READ] & c::LRU_TIME_MAX
        } else {
            0
        }
    }

    /// Its time in @ca's fragmentation lru, 0 for none - how full it is, as a
    /// fraction of 1 << 31, for movable data with room it isn't using:
    /// alloc_lru_idx_fragmentation().
    pub fn lru_idx_fragmentation(&self, ca: &c::bch_dev) -> u64 {
        if self.data_type >= c::BCH_DATA_NR as u8 ||
           c::DATA_TYPES_MOVABLE & (1 << self.data_type) == 0 ||
           self.sectors_fragmented(ca) == 0 {
            return 0;
        }

        // on a corrupt fs, more dirty sectors than the bucket has
        let bucket_size = u64::from(ca.mi.bucket_size);
        let d = self.sectors_dirty().min(bucket_size as i64) as u64;
        d * (1 << 31) / bucket_size
    }

    /// How far its generation is past its oldest pointer's, wrapping:
    /// alloc_gc_gen().
    pub fn gc_gen(&self) -> u8 {
        self.generation.wrapping_sub(self.oldest_gen)
    }
}

// ── Positions in the btrees an alloc key indexes into ────────────────────
//
// need_discard, freespace and bucket_gens are indexes of the alloc btree,
// each with its own encoding of a bucket's position - background.h's
// inlines, for Rust: positions as their own types, converted to and from
// bpos at the btree.

/// A bucket's entry in the need_discard btree: the journal seq it became
/// empty at, then the bucket, as bucket_to_u64().
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NeedDiscardPos {
    pub journal_seq_empty: u64,
    pub bucket:            c::bpos,
}

impl NeedDiscardPos {
    /// Where bucket @bucket, alloc key @a, belongs.
    pub fn new(bucket: c::bpos, a: &c::AllocV4) -> Self {
        Self { journal_seq_empty: a.journal_seq_empty, bucket }
    }

    pub fn from_bpos(p: c::bpos) -> Self {
        Self { journal_seq_empty: p.inode, bucket: u64_to_bucket(p.offset) }
    }

    pub fn to_bpos(self) -> c::bpos {
        pos(self.journal_seq_empty, bucket_to_u64(self.bucket))
    }
}

/// Where in a free bucket's offset, in the freespace btree, the high bits of
/// its gc gen go.
const FREESPACE_GENBITS_SHIFT: u32 = 56;

/// A free bucket's place in the freespace btree: the bucket, with the high
/// bits of its gc gen in the top bits of the offset - alloc_freespace_pos().
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FreespacePos {
    pub bucket:  c::bpos,
    /// The bucket's gc gen >> 4: alloc_freespace_genbits() >> 56. Read from
    /// the btree, whatever the top 8 bits hold.
    pub genbits: u8,
}

impl FreespacePos {
    /// Where bucket @bucket, alloc key @a, belongs.
    pub fn new(bucket: c::bpos, a: &c::AllocV4) -> Self {
        Self { bucket, genbits: a.gc_gen() >> 4 }
    }

    pub fn from_bpos(p: c::bpos) -> Self {
        Self {
            bucket:  pos(p.inode, p.offset & ((1 << FREESPACE_GENBITS_SHIFT) - 1)),
            genbits: (p.offset >> FREESPACE_GENBITS_SHIFT) as u8,
        }
    }

    pub fn to_bpos(self) -> c::bpos {
        pos(self.bucket.inode, self.bucket.offset | u64::from(self.genbits) << FREESPACE_GENBITS_SHIFT)
    }
}

/// Where a bucket's generation is in the bucket_gens btree: the key - one per
/// KEY_TYPE_BUCKET_GENS_NR buckets - and its index in the key's gens[]:
/// alloc_gens_pos().
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BucketGensPos {
    pub key: c::bpos,
    /// < KEY_TYPE_BUCKET_GENS_NR
    pub idx: usize,
}

impl BucketGensPos {
    pub fn new(bucket: c::bpos) -> Self {
        Self {
            key: pos(bucket.inode, bucket.offset >> c::KEY_TYPE_BUCKET_GENS_BITS),
            idx: (bucket.offset & u64::from(c::KEY_TYPE_BUCKET_GENS_MASK)) as usize,
        }
    }

    /// The bucket it's the gen of: bucket_gens_pos_to_alloc().
    pub fn bucket(self) -> c::bpos {
        pos(self.key.inode, self.key.offset << c::KEY_TYPE_BUCKET_GENS_BITS | self.idx as u64)
    }

    /// The bucket's gen in @k, its bucket_gens key - 0 if it hasn't one:
    /// alloc_gen().
    pub fn bucket_gen(self, k: BkeySC<'_>) -> u8 {
        k.arm::<c::BucketGens>().map_or(0, |g| g.v().gens[self.idx])
    }
}
