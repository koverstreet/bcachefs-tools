// SPDX-License-Identifier: GPL-2.0

//! A filesystem's capacity, which devices the allocator writes to, and the
//! free space reservations are made against - background.c's and buckets.c's.
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
//! Free space is counted by the replica count it's placeable at: n copies
//! go to n devices, so lopsided devices place less than they hold
//! (DevFreeDist). Reservations are made in a slot per count, against the
//! global caches in sectors_available and, in front of them, each CPU's -
//! the fastpath, a cmpxchg on this CPU's. When a cache runs short, it
//! refills from the global ones; when they do, a recompute, under
//! sectors_available_lock, zeroes every CPU's cache, recomputes free space,
//! and recomputes the placement: the devices a write at n replicas has to
//! use when free space is lopsided enough that where copies go decides how
//! much fits. What the caches hand out is capped by how far allocation can
//! go before that could change. A commit charges what it adds to usage to
//! its reservation (bch2_trans_account_disk_usage_change()); the write puts
//! the rest.
//!
//! bch_fs_capacity is made by its Init and freed by its drop: its locks, and
//! the per-cpu counters, whose online reservations must all be put by then.

use crate::accounting::{data_type, data_type_movable};
use crate::btree::iter::{BtreeTrans, TransAttempt};
use crate::c;
use crate::c::bch_errcode;
use crate::errcode::{ret_to_c, BchError};
use crate::fs::Fs;
use crate::init::error::trans_inconsistent;
use crate::locking::Lock;
use crate::percpu_field;
use crate::sb::counters::events;
use crate::util::backtrace::prt_symbol;
use crate::util::closure::{closure_wait_event, closure_wake_up};
use crate::util::ffi::Opaque;
use crate::util::kernel::unlikely;
use crate::util::log::{log_no_fs, LogLevel};
use crate::util::percpu::{CpuGuard, PerCpuRef};
use crate::util::rcu::read_lock;
use crate::util::Printbuf;
use c::{DevsMask, PlacementWriter};
use core::cell::Cell;
use core::ffi::{c_int, c_uint, c_ulong};
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, Ordering};
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
                         (ca.mi.get().nbuckets >> 6) +    // copygc
                         3)                         // btree, copygc, rebalance write points
        * u64::from(ca.mi.get().bucket_size) * 2;

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
            bucket_size_max = bucket_size_max.max(u32::from(ca.mi.get().bucket_size));
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
        self.rw_devs_rcu(&rcu).map(|ca| ca.mi.get().nbuckets * u64::from(ca.mi.get().bucket_size)).min().unwrap_or(u64::MAX)
    }
}

// ── Devices the allocator writes to ──────────────────────────────────────

/// @ca in or out of rw_devs[]: every rw device takes free; cached, the
/// user data of a device of durability 0; the rest, what it allows, with
/// durability.
fn dev_allocator_set_rw(fs: &Fs, ca: &c::bch_dev, rw: bool) {
    let allowed = |t: c::bch_data_type| u64::from(ca.mi.get().data_allowed) & (1 << t.0) != 0;

    for (i, devs) in fs.allocator.rw_devs.iter().enumerate() {
        let t = c::bch_data_type(i as u32);
        let data_type_rw = rw && match t {
            c::bch_data_type::BCH_DATA_free   => true,
            c::bch_data_type::BCH_DATA_cached => allowed(c::bch_data_type::BCH_DATA_user) && ca.mi.get().durability == 0,
            _                                 => allowed(t) && ca.mi.get().durability != 0,
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

/// Whether an open bucket on @ca is a write point's - not on the partial
/// list, which doesn't keep writes in flight.
fn dev_has_open_write_point(fs: &Fs, ca: &c::bch_dev) -> bool {
    fs.allocator.open_buckets.iter().any(|ob| {
        let id = ob.id();
        id.valid() && id.dev() == ca.dev_idx && !ob.flag(c::open_bucket::ON_PARTIAL_LIST)
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

// ── Usage ────────────────────────────────────────────────────────────────

/// @r, and the margin charged on top of a reservation.
fn reserve_factor(r: u64) -> u64 {
    r + (r.next_multiple_of(1 << c::RESERVE_FACTOR) >> c::RESERVE_FACTOR)
}

/// reserve_factor()'s inverse: what's free, less the margin.
pub(crate) fn avail_factor(r: u64) -> u64 {
    (r << c::RESERVE_FACTOR) / ((1 << c::RESERVE_FACTOR) + 1)
}

impl Fs {
    /// Sectors in use by kind, every CPU's count summed.
    pub fn usage_base(&self) -> c::bch_fs_usage_base {
        percpu_field!(self.capacity.pcpu.get(), counters.usage).sum_nonnegative()
    }

    /// Sectors reserved for writes in flight, every CPU's count summed.
    pub fn online_reserved(&self) -> u64 {
        percpu_field!(self.capacity.pcpu.get(), counters.online_reserved).sum_nonnegative().iter().sum()
    }

    /// Capacity, and how much of it is used and free, in sectors.
    pub fn usage_read_short(&self) -> c::bch_fs_usage_short {
        let _mark = self.mark_lock().read();
        // one sweep over the CPUs, for both
        let b = percpu_field!(self.capacity.pcpu.get(), counters).sum_nonnegative();

        // a u64's, as C's: nothing keeps hidden below capacity
        let capacity = self.capacity.capacity.get().wrapping_sub(b.usage.hidden);
        let data     = b.usage.data + b.usage.btree;
        // outstanding reservations are counted by replicas; all of them
        let reserved = b.usage.reserved + b.online_reserved.iter().sum::<u64>();
        let used     = capacity.min(data + reserve_factor(reserved));

        c::bch_fs_usage_short { capacity, used, free: capacity - used }
    }
}

/// The usage a committing transaction changes, under mark_lock: added to this
/// CPU's, and an increase charged to its disk reservation - any it doesn't
/// cover taken off the reservation caches, and warned of once.
fn account_disk_usage_change(t: &TransAttempt<'_, '_>) {
    static WARNED: AtomicBool = AtomicBool::new(false);

    let fs = t.fs();
    fs.mark_lock_assert_held();

    let (delta, res) = (t.fs_usage_delta(), t.disk_res());
    let res_sectors = res.map_or(0, |r| r.sectors.get());

    let mut added = delta.btree.wrapping_add(delta.data).wrapping_add(delta.reserved) as i64;

    // Not allowed to reduce sectors_available except by getting a reservation
    let should_not_have_added = added - res_sectors as i64;
    if unlikely(should_not_have_added > 0) {
        // We can't tell which slot these came out of, so take them off all
        // of them: too much self-heals at the next recalc, too little would
        // overcommit
        for a in &fs.capacity.sectors_available {
            let _ = a.fetch_update(Ordering::Relaxed, Ordering::Relaxed,
                                   |old| Some(old.saturating_sub(should_not_have_added as u64)));
        }
        added -= should_not_have_added;
    }

    // what was added never exceeds what the reservation holds: there is one
    if let Some(res) = res.filter(|_| added > 0) {
        // a lost count clamps to slot 0: these were charged elsewhere
        crate::warn_on!(res.nr_replicas.get() == 0);

        res.sectors.set(res.sectors.get() - added as u64);
        fs.online_reserved_at(res.nr_replicas.get()).add((added as u64).wrapping_neg());
    }

    {
        let cpu = CpuGuard::new();
        percpu_field!(fs.capacity.pcpu.this_cpu(&cpu), counters.usage).add(delta);
    }

    if unlikely(should_not_have_added > 0) && !WARNED.swap(true, Ordering::Relaxed) {
        trans_inconsistent(t, &crate::message!(
            "disk usage increased {should_not_have_added} more than {res_sectors} sectors reserved"));
    }
}

/// # Safety
/// @trans is committing.
#[c_export]
pub unsafe fn bch2_trans_account_disk_usage_change(trans: &BtreeTrans<'_>) {
    account_disk_usage_change(&unsafe { trans.attempt_in_progress() })
}

// ── Placeable space ──────────────────────────────────────────────────────

const REPLICAS_MAX: usize = c::BCH_REPLICAS_MAX as usize;

/// A device's free space, in sectors, net of its reserved_sectors - space the
/// allocator will refuse isn't ours to promise.
///
/// @eventual includes the slack in partially used buckets of movable types:
/// reserving against it is what makes a reservation mean the write won't run
/// out of space (POSIX, for fallocate), at the price of maybe waiting on
/// copygc. @now is free buckets only - nothing reserves against it, it says
/// whether writes are about to stall.
#[derive(Clone, Copy, Default)]
struct DevPlaceable {
    now:      u64,
    eventual: u64,
}

impl DevPlaceable {
    fn new(ca: &c::bch_dev) -> Self {
        if !ca.has_capacity() {
            return Self::default();
        }

        let f = ca.usage_full_read_fast();
        let buckets = |t: c::bch_data_type| f.d[t.0 as usize].buckets;
        let bucket_size = u64::from(ca.mi.get().bucket_size);

        // Not the allocator's free or available buckets: those take out open
        // buckets. An open bucket's space is already counted - as free if
        // nothing's been written to it, as fragmented if something has - so
        // taking it out again undercounts, until unmount gives it back.
        // reserved_sectors is our reserve.
        let now = buckets(data_type::free) * bucket_size;
        let reclaimable = buckets(data_type::free) + buckets(data_type::cached) +
            buckets(data_type::need_gc_gens) + buckets(data_type::need_discard);
        let fragmented: u64 = f.d.iter().enumerate()
            .filter(|&(t, _)| data_type_movable(c::bch_data_type(t as u32)))
            .map(|(_, u)| u.fragmented)
            .sum();

        let reserved = ca.reserved_sectors.get();
        Self {
            now:      now.saturating_sub(reserved),
            eventual: (reclaimable * bucket_size + fragmented).saturating_sub(reserved),
        }
    }
}

#[derive(Clone, Copy, Default)]
struct LargestDev {
    free: u64,
    dur:  u32,
    dev:  u32,
}

/// The free space distribution, as much as placeable() needs: the total and
/// the REPLICAS_MAX - 1 largest devices.
///
/// A copy on a device of durability d is min(d, n) of n replicas, so the
/// total is kept at each n, counting each device's free space that many
/// times. A device that can't hold a replica - durability 0, or not rw -
/// isn't in it.
#[derive(Default)]
struct DevFreeDist {
    total:   [u64; REPLICAS_MAX],
    largest: [LargestDev; REPLICAS_MAX - 1],
}

/// required()'s hysteresis: restricting starts a margin before we're in
/// trouble, so every device can be filled, and stops a wider one after.
const PLACEMENT_ENGAGE_SHIFT: u32 = 3;     // start 1/8th early
const PLACEMENT_RELEASE_SHIFT: u32 = 2;    // stop 1/4th late

impl DevFreeDist {
    fn add(&mut self, free: u64, dur: u32, dev: u32) {
        if dur == 0 {
            return;
        }

        for (n, total) in (1..).zip(&mut self.total) {
            *total += free * u64::from(dur.min(n));
        }

        let mut d = LargestDev { free, dur, dev };
        for l in &mut self.largest {
            if d.free > l.free {
                core::mem::swap(&mut d, l);
            }
        }
    }

    /// Physical sectors placeable at @n replicas. A device holds at most one
    /// copy - min(durability, n) of the n - so of P sectors at n replicas the
    /// k largest devices, holding h replicas between them, hold at most hP/n,
    /// and the rest have to hold what's left:
    ///
    /// ```text
    /// P(n - h)/n <= what the rest hold        for h < n
    /// ```
    ///
    /// [2T, 400G, 200G, 200G] at n=3: k=1 leaves 800G for two thirds, k=2
    /// leaves 400G for one third, so 1200G.
    ///
    /// Reservations are n sectors per sector of data whatever the copies land
    /// on - a durability > 1 device doesn't stretch them - so nothing is
    /// placeable at n that isn't at fewer: that's what keeps partition()
    /// exclusive.
    fn placeable(&self, n: usize) -> u64 {
        let mut sum = self.total[n - 1];
        let mut ret = sum.min(if n > 1 { self.placeable(n - 1) } else { u64::MAX });
        let mut held = 0;

        for l in &self.largest {
            let dur = l.dur.min(n as u32) as usize;

            held += dur;
            if held >= n {
                break;
            }

            sum -= sum.min(l.free * dur as u64);
            ret = ret.min((u128::from(sum) * n as u128 / (n - held) as u128) as u64);
        }
        ret
    }

    /// At @n replicas, the devices every write has to put a copy on, and how
    /// far allocation can go before that could change, in physical sectors.
    /// @was is what they were: empty if we weren't restricting.
    ///
    /// A device with more free space than the devices smaller than it have
    /// between them, over the n - 1 other copies of each write, only fills if
    /// every write puts a copy on it: a write that skips it spends the
    /// others' space on data it had room for. (For 2x: more free space than
    /// all the other devices combined.) The next largest likewise against the
    /// smaller ones over n - 2, and so on; it's always a prefix of the n - 1
    /// largest.
    ///
    /// A sector allocated on a smaller device closes the gap by at most one,
    /// so allocating less than the gap can't start a restriction. Frees
    /// aren't covered - freeing space on the device itself also closes it -
    /// but the margin is there for drift.
    fn required(&self, n: usize, was: &DevsMask) -> (DevsMask, u64) {
        let mut required = DevsMask::default();
        let mut allowance = u64::MAX;
        let mut held = 0;

        for (i, l) in self.largest.iter().enumerate() {
            let dur = l.dur.min(n as u32) as usize;

            // replicas each write still needs from smaller devices
            if held + dur >= n {
                break;
            }
            let need = n - held - dur;

            let mut smaller = self.total[need - 1];
            for j in &self.largest[..=i] {
                smaller -= smaller.min(j.free * u64::from(j.dur.min(need as u32)));
            }

            let have    = l.free * need as u64;
            let engage  = smaller - (smaller >> PLACEMENT_ENGAGE_SHIFT);
            let release = smaller - (smaller >> PLACEMENT_RELEASE_SHIFT);

            if have > if was.test(l.dev as usize) { release } else { engage } {
                required.set(l.dev as usize);
                held += dur;
                continue;
            }

            // nothing smaller to spend: allocation can't close the gap
            if smaller != 0 {
                allowance = (engage - have).max(smaller >> (PLACEMENT_ENGAGE_SHIFT + 1));
            }
            break;
        }

        (required, allowance)
    }

    /// Free space partitioned by the highest replica count it supports:
    /// [n - 1] is placeable at n and not n + 1, so no two slots describe the
    /// same sectors. Outstanding reservations - @b's, not yet written, so not
    /// in the bucket counts - come off the way the slowpath drew them, from
    /// their slot upwards; usage.reserved has no count, so it comes off the
    /// top.
    fn partition(&self, b: &c::bch_fs_capacity_counters) -> [u64; REPLICAS_MAX] {
        let mut out = [0; REPLICAS_MAX];
        let mut above = 0;

        for n in (1..=REPLICAS_MAX).rev() {
            let placeable = self.placeable(n);

            // non-increasing in n, or this underflows
            debug_assert!(placeable >= above);

            out[n - 1] = placeable - above;
            above      = placeable;
        }

        // Most constrained first: the top slot's reservations can only have
        // come from the top slot. What doesn't fit was overcommitted, and is
        // dropped.
        for k in (0..REPLICAS_MAX).rev() {
            take_from(&mut out[k..], reserve_factor(b.online_reserved[k]));
        }
        take_from(out.iter_mut().rev(), reserve_factor(b.usage.reserved));

        out.map(avail_factor)
    }
}

/// @todo taken from @slots, in order, as far as they go: what's left.
fn take_from<'a>(slots: impl IntoIterator<Item = &'a mut u64>, mut todo: u64) -> u64 {
    for s in slots {
        if todo == 0 {
            break;
        }
        let take = (*s).min(todo);
        *s   -= take;
        todo -= take;
    }
    todo
}

impl Fs {
    fn free_dists(&self) -> (DevFreeDist, DevFreeDist) {
        let (mut eventual, mut now) = (DevFreeDist::default(), DevFreeDist::default());

        let rcu = read_lock();
        for ca in self.devs_rcu(&rcu) {
            let f = DevPlaceable::new(ca);
            let dur = ca.rw_durability();

            eventual.add(f.eventual, dur, ca.dev_idx.into());
            now.add(f.now, dur, ca.dev_idx.into());
        }
        (eventual, now)
    }

    /// Free space by the replica count it's placeable at, eventually and now,
    /// less what's reserved.
    pub fn sectors_placeable(&self) -> c::bch_fs_placeable {
        let _mark = self.mark_lock().read();
        let (eventual, now) = self.free_dists();

        let b = percpu_field!(self.capacity.pcpu.get(), counters).sum_nonnegative();

        let at_or_above = |d: &DevFreeDist| {
            let mut out = d.partition(&b);
            for i in (0..REPLICAS_MAX - 1).rev() {
                out[i] += out[i + 1];
            }
            out
        };
        c::bch_fs_placeable { eventual: at_or_above(&eventual), now: at_or_above(&now) }
    }
}

// ── Disk reservations ────────────────────────────────────────────────────

pub use c::DiskReservation;

/// What a CPU's cache refills to, past what it's asked for.
const SECTORS_CACHE: u64 = 1024;

/// The slot a reservation at @nr_replicas lives in. Counts above
/// BCH_REPLICAS_MAX are real - durability is per device - and share the last
/// slot, which is the conservative direction. 0 is a bug; the clamp only keeps
/// it in bounds.
fn disk_res_slot(nr_replicas: u32) -> usize {
    debug_assert!(nr_replicas != 0);
    nr_replicas.clamp(1, c::BCH_REPLICAS_MAX as u32) as usize - 1
}

impl Fs {
    /// The count of sectors reserved at @nr_replicas - this CPU's of it, to
    /// add to.
    fn online_reserved_at(&self, nr_replicas: u32) -> PerCpuRef<'_, u64> {
        percpu_field!(self.capacity.pcpu.get(), counters.online_reserved).at(disk_res_slot(nr_replicas))
    }

    /// May we write fewer copies than were asked for, rather than returning
    /// ENOSPC? See enum bch_write_degraded_actions for why the default is
    /// what it is.
    fn write_degraded_ok(&self) -> bool {
        match self.opts.write_degraded() {
            c::bch_write_degraded_actions::BCH_WRITE_DEGRADED_yes => true,
            c::bch_write_degraded_actions::BCH_WRITE_DEGRADED_no  => false,
            _ => !self.flag(c::bch_fs_flags::BCH_FS_all_devs_rw),
        }
    }

    /// A reservation that got @got replicas of the @wanted it asked for:
    /// counted and traced, with @from - where it was asked.
    fn disk_reservation_degraded(&self, wanted: u32, got: u32, sectors: u64, from: impl FnOnce(&mut Printbuf)) {
        self.event_add_trace(events::disk_reservation_degraded, sectors, |out| {
            crate::prt!(out, "wanted {wanted} replicas, got {got}: {sectors} sectors from ");
            from(out);
        });
    }

    /// Every CPU's cache zeroed, and free space and the placement recomputed,
    /// under sectors_available_lock - @placement is its guard's. Every slot,
    /// not just the one that ran short: consuming at one count shrinks the
    /// others, and not linearly, so they only stay honest recomputed
    /// together. Each slot's free space, and how much the caches may hand out
    /// before the placement could change.
    fn reservation_recompute(&self, placement: &PlacementWriter) -> ([u64; REPLICAS_MAX], u64) {
        // A zero racing a CPU's take from its own cache can be lost: what
        // that cache held is handed out twice, as it was with C's
        for caches in percpu_field!(self.capacity.pcpu.get(), sectors_available).each_cpu() {
            caches.iter().for_each(|c| c.set(0));
        }

        let (eventual, _) = self.free_dists();

        let mut allowance = u64::MAX;
        let mut constrained = placement.placement_constrained.get();
        for n in 2..=REPLICAS_MAX {
            let cur = &placement.placement_required[n - 1];
            let (required, a) = eventual.required(n, &cur.get());

            let bit = 1 << (n - 1);
            constrained = if required.is_empty() { constrained & !bit } else { constrained | bit };
            cur.set(required);
            allowance = allowance.min(a);
        }
        placement.placement_constrained.set(constrained);

        (eventual.partition(&percpu_field!(self.capacity.pcpu.get(), counters).sum_nonnegative()), allowance)
    }

    /// The global caches set to @avail, and what the fastpath may hand out
    /// before the next recompute: never more than @budget, so the placement
    /// restriction is re-evaluated before allocation could have changed it.
    /// Capped here and not in the recompute's decision - running out forces a
    /// recompute, never an ENOSPC. Most capable slot first: a reservation at
    /// any count can draw from above its own.
    fn reservation_caches_store(&self, avail: &[u64; REPLICAS_MAX], mut budget: u64) {
        for (cache, &a) in self.capacity.sectors_available.iter().zip(avail).rev() {
            let v = a.min(budget);
            budget -= v;
            cache.store(v, Ordering::Relaxed);
        }
    }

    /// The reservation caches recomputed, after the devices change: they were
    /// computed against the old ones.
    pub fn disk_reservation_caches_invalidate(&self) {
        let mut g = self.capacity.sectors_available_lock.lock();
        let (avail, allowance) = self.reservation_recompute(g.published());
        self.reservation_caches_store(&avail, allowance);
    }

    /// @sectors for @res, when this CPU's cache for its slot is short: the
    /// cache refilled from the global caches - its slot's upwards, least
    /// capable first, leaving scarce high-replica space to writes with no
    /// alternative - and if that's not enough, a recompute. It all lands in
    /// @res's slot's cache: a per-pool cache would never hit for 1x writes,
    /// since slot 1 is empty wherever every device can hold a copy.
    #[cold]
    #[inline(never)]
    fn disk_reservation_add_slowpath(&self, res: &DiskReservation<'_>, sectors: u64,
                                     flags: c::bch_reservation_flags) -> Result<(), BchError> {
        let slot = disk_res_slot(res.nr_replicas.get());
        let want = sectors + SECTORS_CACHE;

        let cpu = CpuGuard::new();
        let pcpu = self.capacity.pcpu.this_cpu(&cpu);
        let cache = percpu_field!(pcpu, sectors_available).at(slot);

        for global in &self.capacity.sectors_available[slot..] {
            let have = cache.get();
            if have >= want {
                break;
            }

            let need = want - have;
            let got = global.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |old| {
                let get = need.min(old);
                (get != 0).then(|| old - get)
            }).map_or(0, |old| need.min(old));

            cache.set(cache.get() + got);
        }

        // A recompute on another CPU may zero our cache: read it once, so the
        // subtract is from what we checked
        let avail = cache.get();

        // the recompute zeroes the caches, so what we drew above isn't lost
        if avail < sectors {
            return self.disk_reservation_add_recompute(res, sectors, flags);
        }

        cache.set(avail - sectors);
        percpu_field!(pcpu, counters.online_reserved).at(slot).add(sectors);
        res.sectors.set(res.sectors.get() + sectors);
        Ok(())
    }

    /// The caches recomputed, and @sectors for @res from what's free at its
    /// count of replicas or more - with PARTIAL, as much as there is; with
    /// NOFAIL, whether there is or not.
    #[cold]
    fn disk_reservation_add_recompute(&self, res: &DiskReservation<'_>, mut sectors: u64,
                                      flags: c::bch_reservation_flags) -> Result<(), BchError> {
        use c::bch_reservation_flags as F;

        let nr_replicas = res.nr_replicas.get();
        let slot = disk_res_slot(nr_replicas);

        let mut g = self.capacity.sectors_available_lock.lock();
        let (mut avail, allowance) = self.reservation_recompute(g.published());

        // anything placeable at more replicas than we need will do
        let have: u64 = avail[slot..].iter().sum();
        if have != 0 && flags.contains(F::BCH_DISK_RESERVATION_PARTIAL) {
            sectors = sectors.min(have);
        }

        let ok = sectors <= have || flags.contains(F::BCH_DISK_RESERVATION_NOFAIL);
        if ok {
            // least capable space first - see DevFreeDist::partition()
            take_from(&mut avail[slot..], sectors);
        }

        self.reservation_caches_store(&avail, allowance);

        if !ok {
            // after a shutdown nothing is writable: that's not out of space
            return Err(self.err(match self.flag(c::bch_fs_flags::BCH_FS_emergency_ro) {
                true  => bch_errcode::BCH_ERR_emergency_ro,
                false => bch_errcode::BCH_ERR_ENOSPC_disk_reservation,
            }));
        }

        self.online_reserved_at(nr_replicas).add(sectors);
        res.sectors.set(res.sectors.get() + sectors);
        Ok(())
    }
}

/// buckets.h's fastpath's, when this CPU's cache is short.
#[c_export]
pub fn bch2_disk_reservation_add_slowpath(c: &Opaque<c::bch_fs>, res: &c::disk_reservation, sectors: u64,
                                          flags: c::bch_reservation_flags) -> c_int {
    ret_to_c(Fs::from_c(c).disk_reservation_add_slowpath(res, sectors, flags))
}

/// buckets.h's bch2_disk_reservation_add()'s, at fewer replicas than asked:
/// @ip is where.
#[c_export]
pub fn bch2_disk_reservation_degraded(c: &Opaque<c::bch_fs>, wanted: c_uint, got: c_uint, sectors: u64, ip: c_ulong) {
    Fs::from_c(c).disk_reservation_degraded(wanted, got, sectors, |out| prt_symbol(out, ip));
}

impl<'f> DiskReservation<'f> {
    pub fn new(fs: &'f Fs) -> Self {
        DiskReservation { c: fs, sectors: Cell::new(0), generation: Cell::new(0), nr_replicas: Cell::new(0) }
    }

    /// To count @nr_replicas: what it holds moves slot with it, the sub and
    /// the add with preemption off between them.
    fn move_slot(&self, nr_replicas: u32) {
        let (old, sectors) = (self.nr_replicas.get(), self.sectors.get());

        if sectors != 0 && disk_res_slot(old) != disk_res_slot(nr_replicas) {
            let _cpu = CpuGuard::new();
            self.c.online_reserved_at(old).add(sectors.wrapping_neg());
            self.c.online_reserved_at(nr_replicas).add(sectors);
        }
        self.nr_replicas.set(nr_replicas);
    }

    /// Charging only ever raises the count: erring high reserves more than we
    /// need, erring low reserves space we can't place. Only write_degraded
    /// lowers it.
    fn set_nr_replicas(&self, nr_replicas: u32) {
        if nr_replicas > self.nr_replicas.get() {
            self.move_slot(nr_replicas);
        }
    }

    /// Reserve @sectors of data at @nr_replicas copies, on top of what it
    /// already holds. On ENOSPC, at fewer copies if write_degraded allows it:
    /// its nr_replicas is then the count to write at.
    #[track_caller]
    pub fn add(&self, sectors: u64, mut nr_replicas: u32, flags: c::bch_reservation_flags) -> Result<(), BchError> {
        let wanted = nr_replicas;
        loop {
            let ret = self.add_physical(sectors * u64::from(nr_replicas), nr_replicas, flags);
            if ret.is_ok() && nr_replicas < wanted {
                let from = core::panic::Location::caller();
                self.c.disk_reservation_degraded(wanted, nr_replicas, sectors, |out| crate::prt!(out, "{from}"));
            }

            match ret {
                Err(e) if e.matches(c::ENOSPC) && nr_replicas > 1 && self.c.write_degraded_ok() => {
                    // the failed attempt set the count; retry one lower
                    nr_replicas -= 1;
                    self.move_slot(nr_replicas);
                }
                r => return r,
            }
        }
    }

    /// Reserve @sectors physical - already weighted by replicas, not data
    /// sectors to multiply - charged at @nr_replicas: for a delta crediting
    /// back overwritten keys at their own counts, sum_sector_overwrites()'s.
    ///
    /// XXX: a smell - that arithmetic probably belongs elsewhere, and when it
    /// goes, so does this.
    pub fn add_physical(&self, sectors: u64, nr_replicas: u32, flags: c::bch_reservation_flags)
        -> Result<(), BchError>
    {
        self.set_nr_replicas(nr_replicas);

        // Not a hard assert: bi_data_replicas isn't validated, so a damaged
        // inode can reach here with a zero count
        debug_assert!(sectors == 0 || self.nr_replicas.get() != 0);

        // nothing to reserve: e.g. fallocate over a range that has enough copies
        if sectors == 0 {
            return Ok(());
        }

        // userspace always takes the slowpath, as C's does
        #[cfg(kernel)]
        if self.add_cached(sectors) {
            return Ok(());
        }
        self.c.disk_reservation_add_slowpath(self, sectors, flags)
    }

    /// @sectors from this CPU's cache for its slot, if it has that many.
    #[cfg(kernel)]
    fn add_cached(&self, sectors: u64) -> bool {
        let nr_replicas = self.nr_replicas.get();
        let cache = percpu_field!(self.c.capacity.pcpu.get(), sectors_available).at(disk_res_slot(nr_replicas)).word();

        let mut old = cache.read();
        loop {
            if sectors > old {
                return false;
            }
            match cache.cmpxchg(old, old - sectors) {
                Ok(_)  => break,
                Err(v) => old = v,
            }
        }

        self.c.online_reserved_at(nr_replicas).add(sectors);
        self.sectors.set(self.sectors.get() + sectors);
        true
    }

    /// Release what it holds, keeping it usable - for a loop reserving per
    /// iteration.
    pub fn put(&self) {
        let sectors = self.sectors.replace(0);
        if sectors != 0 {
            // a lost count clamps to slot 0: these were charged elsewhere
            crate::warn_on!(self.nr_replicas.get() == 0);
            self.c.online_reserved_at(self.nr_replicas.get()).add(sectors.wrapping_neg());
        }
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
            let online_reserved = crate::percpu_field!(self.pcpu.get(), counters.online_reserved).sum();
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
