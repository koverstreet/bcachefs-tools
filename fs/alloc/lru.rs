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

use crate::alloc::buckets::u64_to_bucket;
use crate::btree::bkey::pos;
use crate::c;
use core::ops::RangeInclusive;

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
