// SPDX-License-Identifier: GPL-2.0

//! foreground.h's, for Rust: the open buckets' hash, and whether a bucket is
//! open - being written.
//!
//! The hash is OPEN_BUCKETS_COUNT slots, each the head of a chain through
//! the open buckets' ->hash, 0 ending it, a bucket's slot hash_64() of its
//! (dev, bucket). C finds the slot here too - bch2_open_bucket_hash(), for
//! open_bucket_hashslot() - so the two can't disagree.
//!
//! It's written under freelist_lock and read without it, as C's
//! bch2_bucket_is_open() does: every slot and field once, volatile, as C's
//! plain loads. A bucket opened or closed while we look may be seen or
//! missed: a caller for whom that matters looks again under the lock, as
//! bch2_bucket_is_open_safe().

use crate::c;
use crate::fs::Fs;
use crate::util::hash::{hash_64, Fold};
use core::ffi::c_uint;
use core::ptr::read_volatile;
use cstruct_macros::c_export;

const _: () = assert!(c::OPEN_BUCKETS_COUNT.is_power_of_two());

/// @dev's @bucket's slot in the open buckets' hash.
#[c_export]
pub fn bch2_open_bucket_hash(dev: c_uint, bucket: u64) -> c_uint {
    hash_64(Fold::of(&(dev, bucket)), c::OPEN_BUCKETS_COUNT.ilog2()) as c_uint
}

impl Fs {
    /// Whether @dev's @bucket is open: bch2_bucket_is_open().
    pub fn bucket_is_open(&self, dev: u32, bucket: u64) -> bool {
        let slot = bch2_open_bucket_hash(dev, bucket) as usize;

        // SAFETY: the allocator's, live with the filesystem; each read whole,
        // and racing the writer as C's do
        unsafe {
            let a = &raw const (*self.as_ptr()).allocator;
            let mut ob_idx = read_volatile(&raw const (*a).open_buckets_hash[slot]);
            while ob_idx != 0 {
                let ob = &raw const (*a).open_buckets[ob_idx as usize];
                if u32::from(read_volatile(&raw const (*ob).dev)) == dev &&
                   read_volatile(&raw const (*ob).bucket) == bucket {
                    return true;
                }
                ob_idx = read_volatile(&raw const (*ob).hash);
            }
        }
        false
    }
}
