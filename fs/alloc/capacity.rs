// SPDX-License-Identifier: GPL-2.0

//! A filesystem's capacity, and which devices the allocator writes to -
//! background.c's.
//!
//! Capacity is what the rw devices with durability hold, less what's
//! reserved: each device's reserved_sectors, which the allocator won't hand
//! out, so reservations and free space run out together - their sum is
//! c->capacity.reserved. recalc_capacity() recomputes it all, under
//! state_lock, whenever the devices change.
//!
//! A device the allocator writes to is in c->allocator.rw_devs[] for each
//! data type it takes. Going read-only, it leaves them, its open buckets are
//! stopped, and the writes still in flight to it are waited out.
//!
//! bch_fs_capacity is made by its Init and freed by its drop: its locks, and
//! the per-cpu counters, whose online reservations must all be put by then.

use crate::c;
use crate::errcode::{ret_to_result_void as ret_to_result, BchError};
use crate::fs::Fs;
use crate::locking::Lock;
use crate::util::closure::{closure_wait_event, closure_wake_up};
use crate::util::ffi::Opaque;
use crate::util::log::{log_no_fs, LogLevel};
use crate::util::rcu::read_lock;
use crate::percpu_field;
use core::cell::Cell;
use core::pin::Pin;
use cstruct_macros::c_export;
use pin_init::pinned_drop;

// ── Capacity ─────────────────────────────────────────────────────────────

/// @ca's reserve, of @capacity: enough to refill every reserve from scratch,
/// twice over - copygc spends its whole reserve in one go, then runs again
/// against what that freed - or its share of the copygc reserve, by
/// capacity, not bucket count, as bucket sizes can differ; whichever is
/// more, and never more than the device.
fn dev_reserved_sectors(ca: &c::bch_dev, gc_reserve: u64, capacity: u64) -> u64 {
    let dev_sectors = ca.capacity_sectors();

    let alloc_reserve = (u64::from(ca.nr_btree_reserve) * 2 +
                         (ca.mi.nbuckets >> 6) +    // copygc
                         3)                         // btree, copygc, rebalance write points
        * u64::from(ca.mi.bucket_size) * 2;

    let gc_share = match capacity {
        0 => 0,
        _ => (u128::from(gc_reserve) * u128::from(dev_sectors) / u128::from(capacity)) as u64,
    };

    alloc_reserve.max(gc_share).min(dev_sectors)
}

pub(crate) fn recalc_capacity(fs: &Fs) {
    Lock::from_c(&fs.state_lock).assert_held_any();

    let rcu = read_lock();

    // the total first: the copygc reserve is shared out against it
    let capacity: u64 = fs.devs_rcu(&rcu).filter(|ca| ca.has_capacity()).map(|ca| ca.capacity_sectors()).sum();

    let gc_reserve = match fs.opts.gc_reserve_bytes {
        0 => capacity * u64::from(fs.opts.gc_reserve_percent) / 100,
        b => b >> 9,
    }.min(capacity);

    let (mut reserved, mut bucket_size_max, mut all_devs_rw) = (0, 0, true);
    for ca in fs.devs_rcu(&rcu) {
        all_devs_rw &= ca.is_rw();

        let r = ca.has_capacity().then(|| dev_reserved_sectors(ca, gc_reserve, capacity));
        ca.reserved_sectors.set(r.unwrap_or(0));
        if let Some(r) = r {
            reserved += r;
            bucket_size_max = bucket_size_max.max(u32::from(ca.mi.bucket_size));
        }
    }
    drop(rcu);

    // Cached: every reservation that comes up short asks, and only this
    // changes it
    if all_devs_rw {
        fs.set_flag(c::bch_fs_flags::BCH_FS_all_devs_rw);
    } else {
        fs.clear_flag(c::bch_fs_flags::BCH_FS_all_devs_rw);
    }

    #[cfg(kernel)]
    if !fs.vfs_sb.is_null() {
        // SAFETY: the superblock VFS mounted us on, while it's mounted
        unsafe { (*(*fs.vfs_sb).s_bdi).ra_pages = fs_ra_pages(fs) as _ };
    }

    fs.capacity.reserved.set(reserved);
    fs.capacity.capacity.set(capacity - reserved);
    fs.capacity.bucket_size_max.set(bucket_size_max);

    fs.disk_reservation_caches_invalidate();

    // anyone waiting for buckets
    fs.alloc_wake_all();
}

#[c_export]
pub fn bch2_recalc_capacity(c: &Opaque<c::bch_fs>) {
    recalc_capacity(Fs::from_c(c))
}

/// Pages to read ahead: each device's dev_readahead, or the VM's default.
#[cfg(kernel)]
fn fs_ra_pages(fs: &Fs) -> usize {
    let per_dev = (fs.opts.dev_readahead >> kernel::page::PAGE_SHIFT) as usize;
    let rcu = read_lock();
    // SAFETY: a pointer, read whole - as C's READ_ONCE(): set and cleared
    // as the device comes and goes
    let online = |ca: &c::bch_dev| !unsafe { core::ptr::read_volatile(&raw const ca.disk_sb.bdev) }.is_null();
    match fs.devs_rcu(&rcu).filter(|ca| online(ca)).count() * per_dev {
        // VM_READAHEAD_PAGES
        0 => (128 << 10) >> kernel::page::PAGE_SHIFT,
        n => n,
    }
}

#[cfg(kernel)]
#[c_export]
pub fn bch2_fs_ra_pages(c: &Opaque<c::bch_fs>) -> core::ffi::c_ulong {
    fs_ra_pages(Fs::from_c(c)) as _
}

impl Fs {
    /// The smallest rw device's capacity, in sectors - U64_MAX with none.
    pub fn min_rw_member_capacity(&self) -> u64 {
        let rcu = read_lock();
        self.rw_devs_rcu(&rcu).map(|ca| ca.mi.nbuckets * u64::from(ca.mi.bucket_size)).min().unwrap_or(u64::MAX)
    }
}

// ── Devices the allocator writes to ──────────────────────────────────────

/// @ca in or out of rw_devs[]: every rw device takes free; cached, the
/// user data of a device of durability 0; the rest, what it allows, with
/// durability.
fn dev_allocator_set_rw(fs: &Fs, ca: &c::bch_dev, rw: bool) {
    let allowed = |t: c::bch_data_type| u64::from(ca.mi.data_allowed) & (1 << t.0) != 0;

    for (i, devs) in fs.allocator.rw_devs.iter().enumerate() {
        let t = c::bch_data_type(i as u32);
        let data_type_rw = rw && match t {
            c::bch_data_type::BCH_DATA_free   => true,
            c::bch_data_type::BCH_DATA_cached => allowed(c::bch_data_type::BCH_DATA_user) && ca.mi.durability == 0,
            _                                 => allowed(t) && ca.mi.durability != 0,
        };
        devs.assign(ca.dev_idx.into(), data_type_rw);
    }

    let n = fs.allocator.rw_devs_change_count.get();
    fs.allocator.rw_devs_change_count.set(n.wrapping_add(1));
}

impl Fs {
    /// Device @dev, if there is one, in or out of rw_devs[].
    pub fn dev_allocator_set_rw(&self, dev: u32, rw: bool) {
        if let Some(ca) = self.dev_rcu(dev, &read_lock()) {
            dev_allocator_set_rw(self, ca, rw);
        }
    }
}

#[c_export]
pub fn bch2_dev_allocator_set_rw(c: &Opaque<c::bch_fs>, ca: &c::bch_dev, rw: bool) {
    dev_allocator_set_rw(Fs::from_c(c), ca, rw)
}

/// Whether an open bucket on @ca is a write point's - not on the partial
/// list, which doesn't keep writes in flight.
fn dev_has_open_write_point(fs: &Fs, ca: &c::bch_dev) -> bool {
    fs.allocator.open_buckets.iter().any(|ob| {
        let _l = Lock::from_c(&ob.lock).lock();
        ob.bits.valid() && !ob.bits.on_partial_list() && ob.dev == ca.dev_idx
    })
}

/// @ca going read-only: out of rw_devs[], capacity recomputed without it, its
/// open buckets stopped, and the writes still in flight to it waited for.
#[c_export]
pub fn bch2_dev_allocator_remove(c: &Opaque<c::bch_fs>, ca: &c::bch_dev) {
    let fs = Fs::from_c(c);
    Lock::from_c(&fs.state_lock).assert_held_any();

    dev_allocator_set_rw(fs, ca, false);
    recalc_capacity(fs);

    fs.open_buckets_stop(Some(ca), false);

    // allocations blocked: the device can no longer be allocated from, and
    // capacity has changed
    fs.alloc_wake_all();
    // a journal reservation waiting on space: there may be no devices left
    // to allocate from
    closure_wake_up(&fs.journal.async_wait);

    closure_wait_event(&fs.allocator.open_buckets_wait, || !dev_has_open_write_point(fs, ca));
}

/// @ca going read-write: into rw_devs[].
#[c_export]
pub fn bch2_dev_allocator_add(c: &Opaque<c::bch_fs>, ca: &c::bch_dev) {
    let fs = Fs::from_c(c);
    Lock::from_c(&fs.state_lock).assert_held_any();
    dev_allocator_set_rw(fs, ca, true);
}

// ── Usage and reservations ───────────────────────────────────────────────

/// @r, and the margin charged on top of a reservation: reserve_factor().
fn reserve_factor(r: u64) -> u64 {
    r + (r.next_multiple_of(1 << c::RESERVE_FACTOR) >> c::RESERVE_FACTOR)
}

impl Fs {
    /// The disk reservation caches recomputed, after the devices change:
    /// bch2_disk_reservation_caches_invalidate().
    pub fn disk_reservation_caches_invalidate(&self) {
        unsafe { c::bch2_disk_reservation_caches_invalidate(self.as_ptr()) }
    }

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
        let capacity = self.capacity.capacity.get().wrapping_sub(b.usage.hidden);
        let data     = b.usage.data + b.usage.btree;
        // outstanding reservations are counted by replicas; all of them
        let reserved = b.usage.reserved + b.online_reserved.iter().sum::<u64>();
        let used     = capacity.min(data + reserve_factor(reserved));

        c::bch_fs_usage_short { capacity, used, free: capacity - used }
    }
}

#[c_export]
pub fn bch2_fs_usage_base_read(c: &Opaque<c::bch_fs>) -> c::bch_fs_usage_base {
    Fs::from_c(c).usage_base()
}

#[c_export]
pub fn bch2_fs_usage_read_short(c: &Opaque<c::bch_fs>) -> c::bch_fs_usage_short {
    Fs::from_c(c).usage_read_short()
}

pub use c::DiskReservation;

impl<'f> DiskReservation<'f> {
    pub fn new(fs: &'f Fs) -> Self {
        DiskReservation { c: fs, sectors: Cell::new(0), generation: Cell::new(0), nr_replicas: Cell::new(0) }
    }

    /// Reserve @sectors of data at @nr_replicas copies, on top of what it
    /// already holds: bch2_disk_reservation_add().
    pub fn add(&self, sectors: u64, nr_replicas: u32, flags: c::bch_reservation_flags) -> Result<(), BchError> {
        // SAFETY: its fields are Cells
        ret_to_result(unsafe {
            c::rust_bch2_disk_reservation_add(self.c.as_ptr(), self.as_mut_ptr(), sectors, nr_replicas,
                                              flags.bits() as i32)
        })
    }

    /// Reserve @sectors physical - already weighted by replicas, not data
    /// sectors to multiply - charged at @nr_replicas: __bch2_disk_reservation_add().
    /// For a delta crediting back overwritten keys at their own counts:
    /// sum_sector_overwrites()'s.
    ///
    /// XXX: the space arithmetic its callers do belongs elsewhere - with the
    /// rest of buckets.c in Rust, this goes.
    pub fn add_physical(&self, sectors: u64, nr_replicas: u32, flags: c::bch_reservation_flags)
        -> Result<(), BchError>
    {
        // SAFETY: its fields are Cells
        ret_to_result(unsafe {
            c::rust_bch2_disk_reservation_add_physical(self.c.as_ptr(), self.as_mut_ptr(), sectors, nr_replicas,
                                                       flags.bits() as i32)
        })
    }

    /// Release what it holds, keeping it usable - for a loop reserving per
    /// iteration: bch2_disk_reservation_put().
    pub fn put(&self) {
        // SAFETY: its fields are Cells
        unsafe { c::rust_bch2_disk_reservation_put(self.c.as_ptr(), self.as_mut_ptr()) };
    }

    pub fn as_mut_ptr(&self) -> *mut c::disk_reservation {
        core::ptr::from_ref(self).cast_mut().cast()
    }

    pub fn sectors(&self) -> u64 {
        self.sectors.get()
    }
}

impl Drop for DiskReservation<'_> {
    fn drop(&mut self) {
        self.put();
    }
}

// ── Teardown ─────────────────────────────────────────────────────────────

#[pinned_drop]
impl PinnedDrop for c::bch_fs_capacity {
    fn drop(self: Pin<&mut Self>) {
        if self.pcpu.allocated() {
            let online_reserved = crate::percpu_field!(self.pcpu.get(), online_reserved).sum();
            for (i, &v) in online_reserved.iter().enumerate() {
                if v != 0 {
                    log_no_fs(LogLevel::ERR,
                              &crate::message!("online_reserved not 0 at shutdown: {v} at {} replicas", i + 1));
                    crate::warn_on!(true);
                }
            }
        }
        self.project().pcpu.free();
    }
}
