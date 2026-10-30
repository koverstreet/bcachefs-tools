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
//! trait is what an event's duration goes to besides the stats - c::quantiles,
//! or () - and the TimeStats trait is the stats' methods, written once for a
//! bare bch2_time_stats and a bch2_time_stats_quantiles.
//!
//! The time units durations print in, from nanoseconds to eons, are here
//! too: Nsecs prints in them, as C's bch2_pr_time_units().

use crate::c;
use crate::locking::Guard;
use crate::util::eytzinger::Eytzinger0;
use crate::util::kernel::{local_clock, time_after64};
use crate::util::percpu::{PerCpu, PerCpuRef};
use crate::util::{Nsecs, Printbuf, ToText};
use crate::{percpu_field, warn_on_once};
use core::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use cstruct_macros::c_export;

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

type StatsGuard<'a> = Guard<'a, c::spinlock_t, c::TimeStatsLocked>;

/// What an event's duration goes to besides the stats' own: a
/// bch2_time_stats_quantiles' quantiles, or nothing - ().
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

impl c::quantiles {
    /// The @i'th quantile, in order: QUANTILE_IDX()'s entry's estimate.
    pub fn at(&self, i: u32) -> u64 {
        let e = Eytzinger0::<u32>::new(c::NR_QUANTILES);
        self.entries[e.from_inorder(i) as usize].m.get()
    }

    /// Each in order, none less than the one before.
    fn in_order(&self) -> impl Iterator<Item = u64> + '_ {
        (0..c::NR_QUANTILES).scan(0, |last, i| {
            *last = self.at(i).max(*last);
            Some(*last)
        })
    }
}

/// An estimate per node of an eytzinger tree, stepped toward each duration
/// down the tree from the root until one's at or below it: the steps are
/// C's unsigned ints.
impl Quantiles for c::quantiles {
    fn update(&self, _g: &mut StatsGuard<'_>, v: u64) {
        let mut i = 0;
        while i < c::NR_QUANTILES {
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
            write!(out, "    {q}{}", if i + 1 < c::NR_QUANTILES as usize { ", " } else { "" });
        }
        write!(out, "  ]");
    }
}

impl c::TimeStatsLocked {
    /// None yet: the minimums as large as they come.
    pub fn new() -> Self {
        c::TimeStatsLocked { min_duration: u64::MAX, min_freq: u64::MAX, ..Default::default() }
    }
}

/// An event, from @start to @end.
fn count(g: &mut StatsGuard<'_>, q: &impl Quantiles, start: u64, end: u64) {
    let w = c::TIME_STATS_MV_WEIGHT as u8;

    if time_after64(end, start) {
        let duration = end - start;
        g.duration_stats.update(duration as i64, 0);
        g.duration_stats_weighted.update(duration as i64, w);
        g.max_duration = g.max_duration.max(duration);
        g.min_duration = g.min_duration.min(duration);
        g.total_duration = g.total_duration.wrapping_add(duration);
        q.update(g, duration);
    }

    if g.last_event != 0 && time_after64(end, g.last_event) {
        let freq = end - g.last_event;
        g.freq_stats.update(freq as i64, 0);
        g.freq_stats_weighted.update(freq as i64, w);
        g.max_freq = g.max_freq.max(freq);
        g.min_freq = g.min_freq.min(freq);
    }

    g.last_event = end;
}

/// @b's events counted, and @b emptied - another CPU's, perhaps, racing its
/// owner: only its first nr are real, and nr is bounded, as a bad one
/// mustn't walk off the end.
fn count_buffer(g: &mut StatsGuard<'_>, q: &impl Quantiles, b: &c::time_stat_buffer) {
    let nr = (b.nr.get() as usize).min(b.entries.len());
    for e in &b.entries[..nr] {
        count(g, q, e.start.get(), e.end.get());
    }
    b.nr.set(0);
}

impl c::bch2_time_stats {
    fn buffer(&self) -> Option<PerCpuRef<'_, c::time_stat_buffer>> {
        // update_at()'s release: the buffer zeroed, its locks initialized
        let p = self.buffer.load(Acquire) as *mut c::time_stat_buffer;
        // SAFETY: installed by update_at() and freed by exit() alone, after
        // which nothing uses the stats
        (!p.is_null()).then(|| unsafe { PerCpuRef::from_raw(p) })
    }
}

// ── Time stats, with or without quantiles ────────────────────────────────

/// A bch2_time_stats, and what its events' durations go to besides: the
/// stats, written once for a bare one and a bch2_time_stats_quantiles.
pub trait TimeStats {
    type Quantiles: Quantiles;

    fn stats(&self) -> &c::bch2_time_stats;
    fn quantiles(&self) -> &Self::Quantiles;

    /// An event from @start, a local_clock() time, to now: bch2_time_stats_update().
    fn update(&self, start: u64) {
        self.update_at(start, local_clock())
    }

    /// An event from @start to @end.
    fn update_at(&self, start: u64, end: u64) {
        let (s, q) = (self.stats(), self.quantiles());

        let Some(pcpu) = s.buffer() else {
            let mut g = s.lock.lock();
            count(&mut g, q, start, end);

            // fast enough for a buffer - which another may have installed
            // since we looked; none if it can't be had
            if s.buffer.load(Relaxed) == 0 &&
               g.freq_stats_weighted.median() < 32000 &&
               g.duration_stats.n > 1024 {
                if let Ok(b) = PerCpu::<c::time_stat_buffer>::alloc_gfp(c::GFP_ATOMIC) {
                    percpu_field!(b.get(), lock).init();
                    s.buffer.store(b.into_raw() as usize, Release);
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
            count_buffer(&mut s.lock.lock(), q, &b);
        }
    }

    /// The state it times is now @v - say, blocked or not: a change starts an
    /// event or ends one. Whether one started. Unlocked, as C's
    /// track_event_change(): racing changes may count one event as two, or
    /// none.
    fn track_event_change(&self, v: bool) -> bool {
        let s = self.stats();
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
    fn snapshot(&self) -> c::TimeStatsLocked {
        let s = self.stats();
        let mut g = s.lock.lock();
        if let Some(pcpu) = s.buffer() {
            for b in pcpu.each_cpu() {
                count_buffer(&mut g, self.quantiles(), b);
            }
        }
        *g
    }

    /// As it started, the buffers emptied; not the quantiles.
    fn reset(&self) {
        let s = self.stats();
        let mut g = s.lock.lock();
        *g = c::TimeStatsLocked::new();
        s.last_event_start.set(0);
        if let Some(pcpu) = s.buffer() {
            for b in pcpu.each_cpu() {
                b.nr.set(0);
            }
        }
    }

    /// As JSON.
    fn json_to_text(&self, out: &mut Printbuf) {
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
        self.quantiles().json_to_text(out);
        write!(out, "\n}}\n");
    }

    /// The stats laid out in columns, all time and recent: to_text()'s.
    fn stats_to_text(&self, out: &mut Printbuf) {
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
        self.quantiles().to_text(out);
    }
}

impl TimeStats for c::bch2_time_stats {
    type Quantiles = ();

    fn stats(&self) -> &c::bch2_time_stats {
        self
    }

    fn quantiles(&self) -> &() {
        &()
    }
}

impl TimeStats for c::bch2_time_stats_quantiles {
    type Quantiles = c::quantiles;

    fn stats(&self) -> &c::bch2_time_stats {
        &self.stats
    }

    fn quantiles(&self) -> &c::quantiles {
        &self.quantiles
    }
}

impl ToText for c::bch2_time_stats {
    fn to_text(&self, out: &mut Printbuf) {
        self.stats_to_text(out)
    }
}

impl ToText for c::bch2_time_stats_quantiles {
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
fn mean_and_stddev(out: &mut Printbuf, all: &c::mean_and_variance, recent: &c::mean_and_variance, n: i64) {
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

// ── For C ────────────────────────────────────────────────────────────────

#[c_export]
pub fn bch2_pr_time_units(out: &mut Printbuf, ns: u64) {
    Nsecs(ns).to_text(out)
}

#[c_export]
pub fn bch2_time_stats_to_text(out: &mut Printbuf, stats: &c::bch2_time_stats) {
    stats.to_text(out)
}

#[c_export]
pub fn __bch2_time_stats_update(stats: &c::bch2_time_stats, start: u64, end: u64) {
    stats.update_at(start, end)
}

#[c_export]
pub fn __bch2_time_stats_quantiles_update(statq: &c::bch2_time_stats_quantiles, start: u64, end: u64) {
    statq.update_at(start, end)
}

/// # Safety
/// No one else is using @stats, nor will.
#[c_export]
pub unsafe fn bch2_time_stats_exit(stats: &c::bch2_time_stats) {
    let p = stats.buffer.swap(0, Relaxed) as *mut core::ffi::c_void;
    if !p.is_null() {
        // SAFETY: the caller's; alloc_percpu()'d by update_at()
        unsafe { c::free_percpu(p) };
    }
}
