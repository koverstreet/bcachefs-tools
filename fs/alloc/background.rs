// SPDX-License-Identifier: GPL-2.0

//! Alloc keys - background.c's, as it converts. So far, what check_lrus needs
//! to know where a bucket belongs in the lrus.
//!
//! These are background.h's inlines, for Rust, while C still has its own: the
//! alloc trigger writes lru entries by C's, check_lrus checks them by these.
//! The two must agree exactly - to the C integer types, wrapping and all -
//! or fsck calls good entries bad.

use crate::accounting::{data_type, data_type_from_u8, data_type_is_empty};
use crate::btree::bkey::BkeySC;
use crate::c;
use crate::util::kernel::READ;

/// Alloc key @k, of whatever version, as a current one - one that isn't an
/// alloc key, the slot of a bucket without one, as a zeroed one:
/// bch2_alloc_to_v4().
pub fn alloc_to_v4(k: BkeySC<'_>) -> c::AllocV4 {
    let mut a = c::AllocV4::default();
    // SAFETY: @k is a key, @a an alloc_v4 to fill
    unsafe { c::__bch2_alloc_to_v4(k.into(), &mut a) };
    a
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
}
