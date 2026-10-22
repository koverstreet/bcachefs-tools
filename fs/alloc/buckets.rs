// SPDX-License-Identifier: GPL-2.0

use crate::accounting::data_type_from_u8;
use crate::btree::bkey::pos;
use crate::btree::iter::{TransAttempt, UpdateTriggerFlags};
use crate::c;
use crate::errcode::{ret_to_result_void as ret_to_result, BchError};
use crate::fs::Fs;
use crate::opts::prt_data_type;
use crate::percpu_field;
use crate::util::ffi::Opaque;
use crate::util::percpu::Counters;
use crate::util::rcu::ReadGuard;
use crate::util::Printbuf;
use core::cell::UnsafeCell;
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
