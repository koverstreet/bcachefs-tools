// SPDX-License-Identifier: GPL-2.0

//! Statistics on events that have a duration, cheap enough to collect
//! anywhere - each a start and an end, local_clock() times - and printed on
//! demand: their count; the shortest, longest and total duration, and the
//! shortest and longest time between them; the mean and stddev of both, all
//! time and recent - mean_and_variance.rs's estimators, exponentially
//! weighted.
//!
//! What's under the stats' lock - irqsave, as an event may end in an
//! interrupt - is TimeStatsLocked: the minimums, maximums and estimators. An
//! event is counted under it directly until events come fast enough - more
//! than 1024, a recent median under 32us apart - and then there's a per-cpu
//! buffer: an event goes in this CPU's, under its local lock, and a full
//! buffer is counted under the stats' lock all at once. Printing and reset
//! count or empty every CPU's buffer first - time_stat_buffer has the races.
//!
//! A device's IO latency stats have quantiles too, written under the stats'
//! lock and read without it, by the congestion accounting. The Quantiles
//! trait is what an event's duration goes to besides the stats - quantiles,
//! or () - and TimeStatsKind is a bare TimeStats or a
//! TimeStatsQuantiles.
//!
//! Stats come in a TimeStatsArray, which keeps their buffers' pointers - read
//! by every event - apart from what a flush writes; its get() is a
//! TimeStatsRef, the stats and their pointer, which the methods are on.
//!
//! The time units durations print in, from nanoseconds to eons, are here
//! too: Nsecs prints in them, as C's bch2_pr_time_units().

#![allow(non_camel_case_types)]

use crate::c;
use crate::util::mean_and_variance::MeanAndVariance;
use crate::util::alloc::flags;
use crate::locking::{ExclusiveGuard, Locked, SpinLockIrq};
use crate::data_structures::eytzinger::Eytzinger0;
use crate::os::kernel::{local_clock, time_after64};
use crate::os::percpu::{free_percpu, PerCpu, PerCpuRef, CpuShared};
use crate::util::{Nsecs, Printbuf, ToText};
use crate::{percpu_field, warn_on_once, new_lock};
use core::pin::Pin;
use core::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use cstruct_macros::{bch_enum, MaybeZeroable, Init};
use pin_init::{pinned_drop, pin_data, PinInit};
use crate::errcode::BchError;
use crate::types::CacheAligned;
use crate::util::ffi::Opaque;
use crate::util::projections::Volatile;
use core::ffi::c_uint;
use core::sync::atomic::AtomicUsize;

#[bch_enum(closed)]
#[nr]
#[repr(u32)]
pub enum TimeStat {
    /// Allocate memory in the btree node cache for a new btree node
    btree_node_mem_alloc,
    /// Of that, asking mm for a new node's buffers: both attempts, whether
    /// they succeed or not
    btree_node_buf_alloc,
    /// Split a full btree node into two new nodes
    btree_node_split,
    /// Compact a full btree node on disk
    btree_node_compact,
    /// Merge two adjacent btree nodes
    btree_node_merge,
    /// Sort and resort entire btree nodes in memory, after reading from
    /// disk or for compacting
    btree_node_sort,
    /// Read btree nodes from disk
    btree_node_read,
    /// Post-read btree node processing
    btree_node_read_done,
    /// Write btree node to disk
    btree_node_write,
    /// Foreground time for topology-changing btree updates (splits,
    /// compactions, merges); roughly corresponds to lock held time
    btree_interior_update_foreground,
    /// Total time for topology-changing btree updates, including background
    /// transaction phase after new nodes are written
    btree_interior_update_total,
    /// scan btree node cache for eviction
    btree_node_cache_scan,
    /// scan btree key cache for eviction
    btree_key_cache_scan,
    /// Flush btree write buffer to btree
    btree_write_buffer_flush,
    /// Per-shard: queued to start of execution (workqueue scheduling delay)
    btree_write_buffer_flush_shard_sched_delay,
    /// Per-shard: actual flush work duration (wb_flush_sorted_range)
    btree_write_buffer_flush_shard_work,
    /// GC pass recalculating oldest generation numbers
    btree_gc,
    /// Core write path: allocate space, compress, encrypt, checksum, issue
    /// writes, update extents btree
    data_write,
    /// Core read path: look up extents btree, issue reads, checksum,
    /// decrypt, decompress
    data_read,
    /// Promote: write a cached copy of an extent to promote_target on read
    data_promote,
    /// Flush journal writes: cache flush to devices then FUA journal writes
    journal_flush_write,
    /// Non-flush journal writes, without cache flushes or FUA
    journal_noflush_write,
    /// Flush a journal sequence number to disk for sync, fsync, and bucket
    /// reuse
    journal_flush_seq,
    /// Flush btree journal pins
    journal_pin_flush_btree,
    /// Flush key cache journal pins
    journal_pin_flush_key_cache,
    /// Flush other journal pins
    journal_pin_flush_other,
    /// Write the superblock to every member device; held under sb_lock, so
    /// this is what other superblock writers wait behind
    sb_write,
    /// Blocked: journal reclaim not keeping up with reclaiming space
    blocked_journal_low_on_space,
    /// Blocked: journal pins (dirty btree nodes, key cache entries) not
    /// flushed fast enough
    blocked_journal_low_on_pin,
    /// Blocked: open buckets running low, throttling new journal work so
    /// reclaim can free them
    blocked_journal_low_on_open_buckets,
    /// Blocked: too many journal writes in flight
    blocked_journal_max_in_flight,
    /// Blocked: too many journal entries open, not yet closed for writing
    blocked_journal_max_open,
    /// Blocked: waiting for write buffer flush
    blocked_journal_blocked,
    /// Blocked: writer hit journal_full (no room in current entry, reclaim
    /// not keeping up)
    blocked_journal_full,
    /// Blocked: writer hit journal_pin_full (pin fifo full, btree node /
    /// key cache flushers not keeping up)
    blocked_journal_pin_full,
    /// Blocked: writer hit journal_buf_enomem (preallocated data buffer not
    /// topped up)
    blocked_journal_buf_enomem,
    /// Blocked: writer hit journal_stuck (10s timeout fired in slowpath
    /// wait)
    blocked_journal_stuck,
    /// Blocked: waiting for key cache flush
    blocked_key_cache_flush,
    /// Blocked: bucket allocation waiting, copygc or allocator thread not
    /// keeping up
    blocked_allocate,
    /// Blocked: all open bucket handles in use
    blocked_allocate_open_bucket,
    /// Blocked: write buffer full
    blocked_write_buffer_full,
    /// Blocked: writeback throttle
    blocked_writeback_throttle,
    /// Nocow lock contention
    nocow_lock_contended,
    /// Blocked: discard worker waiting for journal flush to advance
    /// rewind_seq and release buckets
    blocked_discard_journal_flush,
}


/* a TimeStatsQuantiles' - a device's IO latency */
pub const NR_QUANTILES: u32 = 15;

/* written under the stats' lock, read without it - write.c's congestion
 * accounting, the device's congested_to_text() */
#[repr(C)]
#[derive(MaybeZeroable)]
pub struct QuantileEntry {
    pub m: Volatile<u64>,
    pub step: Volatile<u64>,
}

#[repr(C)]
#[derive(MaybeZeroable)]
pub struct quantiles {
    pub entries: [QuantileEntry; NR_QUANTILES as usize],
}

#[repr(C)]
#[derive(MaybeZeroable)]
pub struct TimeStatBufferEntry {
    pub start: Volatile<u64>,
    pub end: Volatile<u64>,
}

#[repr(C)]
#[derive(MaybeZeroable)]
pub struct TimeStatBuffer {
    /*
     * Guards nr and entries for its own cpu: interrupts off - or on
     * PREEMPT_RT a sleeping per-cpu lock, which is what lets the
     * buffer-full flush take the stats' lock under it there. Printing and
     * reset empty other cpus' buffers without it, racing their owners by
     * design, bounded by nr: so nr and the entries are Volatile, and the
     * lock, which its lockers change, Opaque.
     */
    pub lock: Opaque<c::local_lock_t>,
    pub nr: Volatile<c_uint>,
    pub entries: [TimeStatBufferEntry; 31],
}

// SAFETY: Volatiles, and the lock Opaque
unsafe impl CpuShared for TimeStatBufferEntry {}
unsafe impl CpuShared for TimeStatBuffer {}

/* default weight for streaming median+MAD: half-life ≈ 2^N samples */
pub const TIME_STATS_MV_WEIGHT: u32 = 8;

/* the stats' lock's: all in nanoseconds */
#[repr(C)]
#[derive(Default, Clone, Copy, MaybeZeroable)]
#[cfg_attr(test, derive(PartialEq))]
pub struct TimeStatsLocked {
    pub min_duration: u64,
    pub max_duration: u64,
    pub total_duration: u64,
    pub max_freq: u64,
    pub min_freq: u64,
    pub last_event: u64,

    pub duration_stats: MeanAndVariance,
    pub freq_stats: MeanAndVariance,

    pub duration_stats_weighted: MeanAndVariance,
    pub freq_stats_weighted: MeanAndVariance,
}

#[pin_data]
#[repr(C)]
#[derive(MaybeZeroable, Init)]
pub struct TimeStats {
    #[pin]
    #[init(new_lock!(TimeStatsLocked::new(), "&stats->lock"))]
    pub lock: Locked<SpinLockIrq, TimeStatsLocked>,
    /* a state change event in progress since: track_event_change()'s, unlocked */
    pub last_event_start: Volatile<u64>,
}

#[pin_data]
#[repr(C)]
#[derive(MaybeZeroable, Init)]
pub struct TimeStatsQuantiles {
    #[pin]
    #[init(TimeStats::init())]
    pub stats: TimeStats,
    pub quantiles: quantiles,
}

// ── Time units ───────────────────────────────────────────────────────────

const NSEC_PER_SEC: u64 = 1_000_000_000;

/// A unit durations print in, and its length.
#[derive(Clone, Copy)]
struct TimeUnit {
    name:  &'static str,
    nsecs: u64,
}

const TIME_UNITS: [TimeUnit; 10] = {
    const fn u(name: &'static str, nsecs: u64) -> TimeUnit {
        TimeUnit { name, nsecs }
    }
    [
        u("ns",  1),
        u("us",  1_000),
        u("ms",  1_000_000),
        u("s",   NSEC_PER_SEC),
        u("m",   NSEC_PER_SEC * 60),
        u("h",   NSEC_PER_SEC * 3600),
        u("d",   NSEC_PER_SEC * 3600 * 24),
        u("w",   NSEC_PER_SEC * 3600 * 24 * 7),
        /* 365.25 days */
        u("y",   NSEC_PER_SEC * ((3600 * 24 * 365) + (3600 * 24 / 4))),
        u("eon", u64::MAX),
    ]
};

impl TimeUnit {
    /// The largest unit @ns has two of: ns, if none.
    fn of(ns: u64) -> TimeUnit {
        TIME_UNITS.windows(2).find(|w| ns < w[1].nsecs << 1).map_or(TIME_UNITS[TIME_UNITS.len() - 1], |w| w[0])
    }
}

impl ToText for Nsecs {
    fn to_text(&self, out: &mut Printbuf) {
        let u = TimeUnit::of(self.0);
        write!(out, "{} {}", self.0 / u.nsecs, u.name);
    }
}

/// Nsecs, the number right-justified to the tabstop before the unit.
fn nsecs_aligned(out: &mut Printbuf, ns: u64) {
    let u = TimeUnit::of(ns);
    write!(out, "{} \r{}", ns / u.nsecs, u.name);
}

// ── Counting ─────────────────────────────────────────────────────────────

type StatsGuard<'a> = ExclusiveGuard<'a, SpinLockIrq, TimeStatsLocked>;

/// What an event's duration goes to besides the stats' own: a
/// TimeStatsQuantiles' quantiles, or nothing - ().
pub trait Quantiles {
    /// @v, an event's duration, with the stats' lock held: @g.
    fn update(&self, g: &mut StatsGuard<'_>, v: u64);
    /// Their line, at the end of the stats' to_text().
    fn to_text(&self, out: &mut Printbuf);
    /// Their part of the JSON, after its last object.
    fn json_to_text(&self, out: &mut Printbuf);
}

impl Quantiles for () {
    fn update(&self, _g: &mut StatsGuard<'_>, _v: u64) {}
    fn to_text(&self, _out: &mut Printbuf) {}
    fn json_to_text(&self, _out: &mut Printbuf) {}
}

impl quantiles {
    /// The @i'th quantile, in order: QUANTILE_IDX()'s entry's estimate.
    pub fn at(&self, i: u32) -> u64 {
        let e = Eytzinger0::<u32>::new(NR_QUANTILES);
        self.entries[e.from_inorder(i) as usize].m.get()
    }

    /// Each in order, none less than the one before.
    fn in_order(&self) -> impl Iterator<Item = u64> + '_ {
        (0..NR_QUANTILES).scan(0, |last, i| {
            *last = self.at(i).max(*last);
            Some(*last)
        })
    }
}

/// An estimate per node of an eytzinger tree, stepped toward each duration
/// down the tree from the root until one's at or below it: the steps are
/// C's unsigned ints.
impl Quantiles for quantiles {
    fn update(&self, _g: &mut StatsGuard<'_>, v: u64) {
        let mut i = 0;
        while i < NR_QUANTILES {
            let e = &self.entries[i as usize];
            let (mut m, mut step) = (e.m.get(), e.step.get());

            if step == 0 {
                m = v;
                step = u64::from(((v / 2) as u32).max(1024));
            } else if m > v {
                m = m.saturating_sub(step);
            } else if m < v {
                m = m.checked_add(step).unwrap_or(u64::from(u32::MAX));
            }

            if m.abs_diff(v) < step {
                step = u64::from(((step / 2) as u32).max(1));
            }
            e.m.set(m);
            e.step.set(step);

            if v >= m {
                break;
            }
            i = Eytzinger0::<u32>::child(i, u32::from(v > m));
        }
    }

    fn to_text(&self, out: &mut Printbuf) {
        let u = TimeUnit::of(self.at(0));
        write!(out, "quantiles ({}):\t", u.name);
        for q in self.in_order() {
            write!(out, "{} ", q / u.nsecs);
        }
        out.newline();
    }

    fn json_to_text(&self, out: &mut Printbuf) {
        write!(out, ",\n  \"quantiles_ns\": [\n");
        for (i, q) in self.in_order().enumerate() {
            write!(out, "    {q}{}", if i + 1 < NR_QUANTILES as usize { ", " } else { "" });
        }
        write!(out, "  ]");
    }
}

impl TimeStatsLocked {
    /// None yet: the minimums as large as they come.
    pub fn new() -> Self {
        TimeStatsLocked { min_duration: u64::MAX, min_freq: u64::MAX, ..Default::default() }
    }
}

/// An event's duration, if it ended after it started.
fn duration(start: u64, end: u64) -> Option<u64> {
    time_after64(end, start).then(|| end - start)
}

/// The time from @last, the end of the event before - if there was one - to
/// @end.
fn since(last: u64, end: u64) -> Option<u64> {
    (last != 0 && time_after64(end, last)).then(|| end - last)
}

/// An event's steps of the estimators and quantiles: what must be counted in
/// order.
fn step(g: &mut StatsGuard<'_>, q: &impl Quantiles, duration: Option<u64>, freq: Option<u64>) {
    let w = TIME_STATS_MV_WEIGHT as u8;

    if let Some(d) = duration {
        g.duration_stats.update(d as i64, 0);
        g.duration_stats_weighted.update(d as i64, w);
        q.update(g, d);
    }
    if let Some(f) = freq {
        g.freq_stats.update(f as i64, 0);
        g.freq_stats_weighted.update(f as i64, w);
    }
}

/// An event, from @start to @end.
fn count(g: &mut StatsGuard<'_>, q: &impl Quantiles, start: u64, end: u64) {
    let (d, f) = (duration(start, end), since(g.last_event, end));
    step(g, q, d, f);

    if let Some(d) = d {
        g.max_duration = g.max_duration.max(d);
        g.min_duration = g.min_duration.min(d);
        g.total_duration = g.total_duration.wrapping_add(d);
    }
    if let Some(f) = f {
        g.max_freq = g.max_freq.max(f);
        g.min_freq = g.min_freq.min(f);
    }
    g.last_event = end;
}

/// @b's events counted, and @b emptied - another CPU's, perhaps, racing its
/// owner: only its first nr are real, and nr is bounded, as a bad one
/// mustn't walk off the end.
fn count_buffer(g: &mut StatsGuard<'_>, q: &impl Quantiles, b: &TimeStatBuffer) {
    let nr = (b.nr.get() as usize).min(b.entries.len());
    for e in &b.entries[..nr] {
        count(g, q, e.start.get(), e.end.get());
    }
    b.nr.set(0);
}

/// What a full buffer's events add to the minimums, maximums and total -
/// and the times between them, after the first - worked out before taking
/// the stats' lock: count_full_buffer() then only steps the estimators.
struct BufferSummary {
    first_end:      u64,
    last_end:       u64,
    min_duration:   u64,
    max_duration:   u64,
    total_duration: u64,
    min_freq:       u64,
    max_freq:       u64,
}

impl BufferSummary {
    fn new(b: &TimeStatBuffer) -> Self {
        let mut s = BufferSummary {
            first_end:      b.entries[0].end.get(),
            last_end:       0,
            min_duration:   u64::MAX,
            max_duration:   0,
            total_duration: 0,
            min_freq:       u64::MAX,
            max_freq:       0,
        };
        for e in &b.entries {
            let end = e.end.get();
            if let Some(d) = duration(e.start.get(), end) {
                s.min_duration = s.min_duration.min(d);
                s.max_duration = s.max_duration.max(d);
                s.total_duration = s.total_duration.wrapping_add(d);
            }
            if let Some(f) = since(s.last_end, end) {
                s.min_freq = s.min_freq.min(f);
                s.max_freq = s.max_freq.max(f);
            }
            s.last_end = end;
        }
        s
    }
}

/// @b's events counted, @b full and this CPU's: as count() of each, in order.
fn count_full_buffer(g: &mut StatsGuard<'_>, q: &impl Quantiles, b: &TimeStatBuffer, s: &BufferSummary) {
    if let Some(f) = since(g.last_event, s.first_end) {
        g.min_freq = g.min_freq.min(f);
        g.max_freq = g.max_freq.max(f);
    }

    let mut last = g.last_event;
    for e in &b.entries {
        let end = e.end.get();
        step(g, q, duration(e.start.get(), end), since(last, end));
        last = end;
    }

    g.min_duration = g.min_duration.min(s.min_duration);
    g.max_duration = g.max_duration.max(s.max_duration);
    g.total_duration = g.total_duration.wrapping_add(s.total_duration);
    g.min_freq = g.min_freq.min(s.min_freq);
    g.max_freq = g.max_freq.max(s.max_freq);
    g.last_event = s.last_end;
}

// ── Time stats, with or without quantiles ────────────────────────────────

/// A TimeStats, or one with quantiles: the stats, and what its events'
/// durations go to besides.
pub trait TimeStatsKind: Sized {
    type Quantiles: Quantiles;

    fn init() -> impl PinInit<Self, BchError>;
    fn stats(&self) -> &TimeStats;
    fn quantiles(&self) -> &Self::Quantiles;
}

impl TimeStatsKind for TimeStats {
    type Quantiles = ();

    fn init() -> impl PinInit<Self, BchError> {
        TimeStats::init()
    }

    fn stats(&self) -> &TimeStats {
        self
    }

    fn quantiles(&self) -> &() {
        &()
    }
}

impl TimeStatsKind for TimeStatsQuantiles {
    type Quantiles = quantiles;

    fn init() -> impl PinInit<Self, BchError> {
        TimeStatsQuantiles::init()
    }

    fn stats(&self) -> &TimeStats {
        &self.stats
    }

    fn quantiles(&self) -> &quantiles {
        &self.quantiles
    }
}

/// @N time stats, and their per-cpu buffers: each event reads its stats'
/// buffer pointer, so the pointers - written once - are packed together on
/// lines of their own, not on the ones a buffer flush writes.
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct TimeStatsArray<T, const N: usize> {
    /* each stats' buffer, once events come fast enough: installed under the
     * stats' lock, read without it - released and acquired, for what's in
     * it - and freed when the array drops */
    buffers: [AtomicUsize; N],
    _align: CacheAligned,
    #[pin]
    stats: [T; N],
}

/* by hand: MaybeZeroable takes no const generics */
unsafe impl<T: pin_init::Zeroable, const N: usize> pin_init::Zeroable for TimeStatsArray<T, N> {}

impl<T: TimeStatsKind, const N: usize> TimeStatsArray<T, N> {
    pub fn new() -> impl PinInit<Self, BchError> {
        pin_init::pin_init!(Self {
            buffers: [const { AtomicUsize::new(0) }; N],
            _align: CacheAligned::default(),
            stats <- pin_init::pin_init_array_from_fn(|_| T::init()),
        }? BchError)
    }

    pub fn get(&self, i: usize) -> TimeStatsRef<'_, T> {
        TimeStatsRef { stats: &self.stats[i], buffer: &self.buffers[i] }
    }
}

#[pinned_drop]
impl<T, const N: usize> PinnedDrop for TimeStatsArray<T, N> {
    fn drop(self: Pin<&mut Self>) {
        for b in &self.buffers {
            let p = b.load(Relaxed) as *mut core::ffi::c_void;
            if !p.is_null() {
                // SAFETY: alloc_percpu()'d by update_at(), and ours: the stats
                // are going
                unsafe { free_percpu(p) };
            }
        }
    }
}

/// One of a TimeStatsArray's stats, and its buffer.
pub struct TimeStatsRef<'a, T> {
    stats:  &'a T,
    buffer: &'a AtomicUsize,
}

impl<T> Clone for TimeStatsRef<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for TimeStatsRef<'_, T> {}

impl<'a, T: TimeStatsKind> TimeStatsRef<'a, T> {
    pub fn stats(self) -> &'a T {
        self.stats
    }

    fn buffer(self) -> Option<PerCpuRef<'a, TimeStatBuffer>> {
        // update_at()'s release: the buffer zeroed, its locks initialized
        let p = self.buffer.load(Acquire) as *mut TimeStatBuffer;
        // SAFETY: installed by update_at(), and freed only when the array
        // drops
        (!p.is_null()).then(|| unsafe { PerCpuRef::from_raw(p) })
    }

    /// An event from @start, a local_clock() time, to now: bch2_time_stats_update().
    pub fn update(self, start: u64) {
        self.update_at(start, local_clock())
    }

    /// An event from now to when the Timer's dropped.
    pub fn timer(self) -> Timer<'a, T> {
        Timer { stats: self, start: local_clock() }
    }

    /// @f, timed as an event.
    pub fn time<R>(self, f: impl FnOnce() -> R) -> R {
        let _timer = self.timer();
        f()
    }

    /// An event from @start to @end.
    pub fn update_at(self, start: u64, end: u64) {
        let (s, q) = (self.stats.stats(), self.stats.quantiles());

        let Some(pcpu) = self.buffer() else {
            let mut g = s.lock.lock();
            count(&mut g, q, start, end);

            // fast enough for a buffer - which another may have installed
            // since we looked; none if it can't be had
            if self.buffer.load(Relaxed) == 0 &&
               g.freq_stats_weighted.median() < 32000 &&
               g.duration_stats.n > 1024 {
                if let Ok(b) = PerCpu::<TimeStatBuffer>::alloc_gfp(flags::ATOMIC) {
                    percpu_field!(b.get(), lock).init();
                    self.buffer.store(b.into_raw() as usize, Release);
                }
            }
            return;
        };

        let lg = percpu_field!(pcpu, lock).lock_irqsave();
        let b = pcpu.this_cpu(&lg);

        /*
         * The update paths can't do this - they're locked, and flush at
         * exactly capacity - so a bad nr means the buffer was scribbled on or
         * freed under us. Seen in the field (7.1.3, read completion): note it
         * and start over, rather than take down the machine for a stats
         * glitch.
         */
        let mut nr = b.nr.get() as usize;
        if warn_on_once!(nr >= b.entries.len()) {
            nr = 0;
        }
        b.entries[nr].start.set(start);
        b.entries[nr].end.set(end);
        b.nr.set((nr + 1) as u32);

        if nr + 1 == b.entries.len() {
            let summary = BufferSummary::new(&b);
            let mut g = s.lock.lock();
            /* a snapshot may have counted them since: then nr's 0 */
            if b.nr.get() as usize == b.entries.len() {
                count_full_buffer(&mut g, q, &b, &summary);
            }
            b.nr.set(0);
        }
    }

    /// The state it times is now @v - say, blocked or not: a change starts an
    /// event or ends one. Whether one started. Unlocked, as C's
    /// track_event_change(): racing changes may count one event as two, or
    /// none.
    pub fn track_event_change(self, v: bool) -> bool {
        let s = self.stats.stats();
        let start = s.last_event_start.get();
        if v == (start != 0) {
            return false;
        }
        if v {
            s.last_event_start.set(local_clock().max(1));
        } else {
            self.update(start);
            s.last_event_start.set(0);
        }
        v
    }

    /// Every CPU's buffer counted, and what's under the lock as of then: to
    /// print, without the lock - the printbuf allocates.
    fn snapshot(self) -> TimeStatsLocked {
        let s = self.stats.stats();
        let mut g = s.lock.lock();
        if let Some(pcpu) = self.buffer() {
            for b in pcpu.each_cpu() {
                count_buffer(&mut g, self.stats.quantiles(), b);
            }
        }
        *g
    }

    /// As it started, the buffers emptied; not the quantiles.
    pub fn reset(self) {
        let s = self.stats.stats();
        let mut g = s.lock.lock();
        *g = TimeStatsLocked::new();
        s.last_event_start.set(0);
        if let Some(pcpu) = self.buffer() {
            for b in pcpu.each_cpu() {
                b.nr.set(0);
            }
        }
    }

    /// As JSON.
    pub fn json_to_text(self, out: &mut Printbuf) {
        let s = self.snapshot();
        let (d, f) = (&s.duration_stats, &s.freq_stats);
        let (dw, fw) = (&s.duration_stats_weighted, &s.freq_stats_weighted);
        let (d_mean, d_stddev, f_mean, f_stddev) = if f.n != 0 {
            (d.mean() as u64, d.stddev().into(), f.mean() as u64, f.stddev().into())
        } else {
            (0, 0, 0, 0)
        };
        let min = |v: u64| if v != u64::MAX { v } else { 0 };

        write!(out, "{{\n  \"count\":       {},\n", d.n);
        json_obj(out, "duration_ns", &[("min", min(s.min_duration)), ("max", s.max_duration),
                                       ("total", s.total_duration), ("mean", d_mean), ("stddev", d_stddev)]);
        write!(out, ",\n");
        json_obj(out, "duration_ewma_ns", &[("mean", dw.median() as u64), ("stddev", dw.stddev().into())]);
        write!(out, ",\n");
        json_obj(out, "between_ns", &[("min", min(s.min_freq)), ("max", s.max_freq),
                                      ("mean", f_mean), ("stddev", f_stddev)]);
        write!(out, ",\n");
        json_obj(out, "between_ewma_ns", &[("mean", fw.median() as u64), ("stddev", fw.stddev().into())]);
        self.stats.quantiles().json_to_text(out);
        write!(out, "\n}}\n");
    }

    /// The stats laid out in columns, all time and recent: to_text()'s.
    fn stats_to_text(self, out: &mut Printbuf) {
        let s = self.snapshot();
        /* the freq stats' count, as C's: none, no means */
        let n = s.freq_stats.n;

        out.tabstop_push(out.indent_level() + TABSTOP_SIZE);
        write!(out, "count:\t{}\n", s.duration_stats.n);
        out.tabstops_reset();

        out.tabstops(&[out.indent_level() + 20, TABSTOP_SIZE + 2, 0, TABSTOP_SIZE + 2]);
        write!(out, "\tsince mount\r\trecent\r\n");

        out.tabstops(&[out.indent_level() + 20, TABSTOP_SIZE, 2, TABSTOP_SIZE]);

        write!(out, "duration of events\n");
        {
            let mut out = out.indent(2);
            name_and_nsecs(&mut out, "min:", s.min_duration);
            name_and_nsecs(&mut out, "max:", s.max_duration);
            name_and_nsecs(&mut out, "total:", s.total_duration);
            mean_and_stddev(&mut out, &s.duration_stats, &s.duration_stats_weighted, n);
        }

        write!(out, "time between events\n");
        {
            let mut out = out.indent(2);
            name_and_nsecs(&mut out, "min:", s.min_freq);
            name_and_nsecs(&mut out, "max:", s.max_freq);
            mean_and_stddev(&mut out, &s.freq_stats, &s.freq_stats_weighted, n);
        }

        out.tabstops_reset();
        self.stats.quantiles().to_text(out);
    }
}

/// An event in progress, counted when it's dropped: one timed to the end of a
/// scope, however the scope's left.
pub struct Timer<'a, T: TimeStatsKind> {
    stats: TimeStatsRef<'a, T>,
    start: u64,
}

impl<T: TimeStatsKind> Drop for Timer<'_, T> {
    fn drop(&mut self) {
        self.stats.update(self.start)
    }
}

impl<T: TimeStatsKind> ToText for TimeStatsRef<'_, T> {
    fn to_text(&self, out: &mut Printbuf) {
        self.stats_to_text(out)
    }
}

const TABSTOP_SIZE: u32 = 12;

/// An object of the JSON, @name: @fields, their values aligned.
fn json_obj(out: &mut Printbuf, name: &str, fields: &[(&str, u64)]) {
    write!(out, "  \"{name}\": {{\n");
    for (i, (k, v)) in fields.iter().enumerate() {
        let pad = 10 - k.len();
        write!(out, "    \"{k}\":{:pad$}{v}{}\n", "", if i + 1 < fields.len() { "," } else { "" });
    }
    write!(out, "  }}");
}

/// @name and @ns, aligned.
fn name_and_nsecs(out: &mut Printbuf, name: &str, ns: u64) {
    write!(out, "{name}\t");
    nsecs_aligned(out, ns);
    out.newline();
}

/// The mean and stddev lines of @all and @recent, all time and recent: no
/// mean if there's no @n.
fn mean_and_stddev(out: &mut Printbuf, all: &MeanAndVariance, recent: &MeanAndVariance, n: i64) {
    let (mean, stddev) = if n != 0 { (all.mean(), u64::from(all.stddev())) } else { (0, 0) };

    write!(out, "mean:\t");
    nsecs_aligned(out, mean as u64);
    out.tab();
    nsecs_aligned(out, recent.median() as u64);
    out.newline();

    write!(out, "stddev:\t");
    nsecs_aligned(out, stddev);
    out.tab();
    nsecs_aligned(out, u64::from(recent.stddev()));
    out.newline();
}


#[cfg(test)]
mod tests {
    use super::*;
    use pin_init::stack_try_pin_init;

    /// A full buffer's events, counted as one, as count() counts each: the
    /// same stats and quantiles, over buffers of random events - some of no
    /// duration, some out of order.
    #[test]
    fn full_buffer_counts_as_each() -> Result<(), BchError> {
        stack_try_pin_init!(let each =? TimeStatsQuantiles::init());
        stack_try_pin_init!(let full =? TimeStatsQuantiles::init());
        let b: TimeStatBuffer = pin_init::zeroed();

        let mut x = 0x2545_f491_4f6c_dd1du64;
        let mut rand = move |m: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % m
        };

        let mut now = 1 << 32;
        for _ in 0..1000 {
            for e in &b.entries {
                now += rand(4096);
                let end = if rand(16) == 0 { now - rand(8192) } else { now };
                let start = if rand(16) == 0 { end + rand(100) } else { end - rand(10000) };
                e.start.set(start);
                e.end.set(end);
            }

            {
                let mut g = each.stats.lock.lock();
                for e in &b.entries {
                    count(&mut g, &each.quantiles, e.start.get(), e.end.get());
                }
            }
            let summary = BufferSummary::new(&b);
            count_full_buffer(&mut full.stats.lock.lock(), &full.quantiles, &b, &summary);

            assert!(*each.stats.lock.lock() == *full.stats.lock.lock());
            for i in 0..NR_QUANTILES {
                assert_eq!(each.quantiles.at(i), full.quantiles.at(i));
            }
        }
        Ok(())
    }
}
