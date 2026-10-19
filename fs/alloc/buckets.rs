// SPDX-License-Identifier: GPL-2.0

use crate::btree::bkey::pos;
use crate::c;
use crate::errcode::{ret_to_result_void as ret_to_result, BchError};
use crate::fs::Fs;
use crate::util::rcu::ReadGuard;
use core::cell::UnsafeCell;

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

// ── Usage ────────────────────────────────────────────────────────────────

impl c::bch_dev {
    /// Its buckets by data type, every CPU's count summed - not under any
    /// lock, so only near enough: bch2_dev_usage_read_fast().
    pub fn usage_read_fast(&self) -> c::bch_dev_usage {
        let mut u = c::bch_dev_usage::default();
        unsafe { c::bch2_dev_usage_read_fast(core::ptr::from_ref(self).cast_mut(), &mut u) };
        u
    }
}

// ── Bucket generations ───────────────────────────────────────────────────

/// How far generation @a is past @b, or 0 if it isn't - wrapping, as gens
/// do: gen_after().
pub fn gen_after(a: u8, b: u8) -> u8 {
    (a.wrapping_sub(b) as i8).max(0) as u8
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
