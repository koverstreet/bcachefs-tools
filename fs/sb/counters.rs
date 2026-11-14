// SPDX-License-Identifier: GPL-2.0

//! Counters - sb/counters.h's event_inc() and event_add() - for Rust; C's
//! macros are as they were.
//!
//! A counter is one of BCH_PERSISTENT_COUNTERS() (counters_format.rs), in the
//! filesystem's counters.now[], per CPU, with the tracepoint of the same name
//! every counter has (debug/trace.rs). Its type, events::<name>, carries what
//! it counts - occurrences or sectors - so
//! event_inc() takes the one and event_add() the other, as C's
//! counter_typecheck() checks.
//!
//! Counters are util/percpu.rs's PerCpu, added to on this CPU.

#![allow(non_camel_case_types)]

use cstruct_macros::{MaybeZeroable, TypeInfo};
use crate::c;
use crate::debug::trace::{events, TraceEvent};
use crate::errcode::BchError;
use crate::util::vstructs::Unvalidated;
use crate::fs::{Fs, SbLockGuard};
use crate::prt;
use crate::sb::io::{sb_field_u64s, SbFieldOps};
use crate::types::{Le64, c_default};
use crate::os::kernel::{nsecs_to_jiffies, NSEC_PER_SEC};
use crate::util::workqueue::Queue;
use crate::util::Printbuf;
use crate::sb::io::SbResize;
use crate::util::percpu::PerCpu;
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

pub const NR_RECENT_COUNTERS: u32 = 20;

#[repr(C)]
#[derive(MaybeZeroable, TypeInfo)]
pub struct bch_fs_counters {
    mount: [AtomicU64; c::PersistentCounter::NR as usize],
    pub now: PerCpu<[u64; c::PersistentCounter::NR as usize]>,

    recent: [[AtomicU64; c::PersistentCounter::NR as usize]; c::NR_RECENT_COUNTERS as usize],
}
c_default!(bch_fs_counters);

/// What an event's counter counts.
pub trait Kind {}

/// Occurrences, counted with event_inc().
pub enum Count {}

/// An amount in sectors, added with event_add().
pub enum Sectors {}

impl Kind for Count {}
impl Kind for Sectors {}

/// One of BCH_PERSISTENT_COUNTERS(): its tracepoint, and its counter.
pub trait Event: TraceEvent {
    type Kind: Kind;

    /// Its counter's index in counters.now[]: BCH_COUNTER_<name>.
    const IDX: usize;
}

macro_rules! event_kind {
    (counter) => { Count };
    (sectors) => { Sectors };
}

macro_rules! counter_events {
    ($(($name:ident, $nr:expr, [$id:tt], [$ty:tt])),* $(,)?) => {
        $(
            impl Event for events::$name {
                type Kind = event_kind!($ty);

                const IDX: usize = c::PersistentCounter::$name as usize;
            }
        )*
    };
}
c::PersistentCounter_arms!(counter_events);

pub struct CounterInfo {
    pub name: &'static str,
    pub stable_id: u16,
    pub is_sectors: bool,
}

// From PersistentCounter_arms!(): (name, index, [stable id], [type]).
macro_rules! counter_is_sectors {
    (sectors) => { true };
    ($other:tt) => { false };
}
macro_rules! counter_table {
    ($(($name:tt, $nr:expr, [$id:tt], [$ty:tt])),* $(,)?) => {
        pub const COUNTERS: &[CounterInfo] = &[$(
            CounterInfo { name: stringify!($name), stable_id: $id, is_sectors: counter_is_sectors!($ty) },
        )*];
    };
}
c::PersistentCounter_arms!(counter_table);

impl Fs {
    /// Count @_e once: event_inc().
    #[inline(always)]
    pub fn event_inc<E: Event<Kind = Count>>(&self, _e: E) {
        self.event_count::<E>(1);
    }

    /// Add @nr sectors to @_e: event_add().
    #[inline(always)]
    pub fn event_add<E: Event<Kind = Sectors>>(&self, _e: E, nr: u64) {
        self.event_count::<E>(nr);
    }

    #[inline(always)]
    fn event_count<E: Event>(&self, nr: u64) {
        self.counters.now.at(E::IDX).add(nr);
    }

    /// event_trace(), then event_inc(): event_inc_trace().
    #[inline(always)]
    pub fn event_inc_trace<E: Event<Kind = Count>>(&self, e: E, msg: impl FnOnce(&mut Printbuf)) {
        self.event_trace(e, msg);
        self.event_inc(e);
    }

    /// event_trace(), then event_add(): event_add_trace().
    #[inline(always)]
    pub fn event_add_trace<E: Event<Kind = Sectors>>(&self, e: E, nr: u64,
                                                    msg: impl FnOnce(&mut Printbuf)) {
        self.event_trace(e, msg);
        self.event_add(e, nr);
    }
}

// ── The counters, in memory and in the superblock ──────────────────────────
//
// In memory by BCH_COUNTER_* index, COUNTERS' order; in the superblock and the
// ioctl by stable id. mount[] and recent[] are written racing readers that
// only print them, as in C: each a u64 on its own, so relaxed atomics.

const NR: usize = c::PersistentCounter::NR as usize;
const NR_STABLE: usize = c::PersistentCounterStable::NR as usize;

impl c::bch_fs_counters {
    /// Counter @i, every CPU's count summed.
    fn now(&self, i: usize) -> u64 {
        self.now.at(i).sum()
    }

    /// Counter @i set to @v: an event counted as it's set may as well not
    /// have been.
    fn set_now(&self, i: usize, v: u64) {
        unsafe { self.now.at(i).set(v) }
    }

    /// Counter @i at mount.
    pub fn mount(&self, i: usize) -> u64 {
        self.mount[i].load(Relaxed)
    }

    /// The counts now, as recent[]'s newest sample, the rest moved down one.
    fn sample(&self) {
        for j in (1..self.recent.len()).rev() {
            for (to, from) in self.recent[j].iter().zip(&self.recent[j - 1]) {
                to.store(from.load(Relaxed), Relaxed);
            }
        }
        for (i, v) in self.recent[0].iter().enumerate() {
            v.store(self.now(i), Relaxed);
        }
    }

    /// What's relative to counter @i zeroed with it: its count at mount and
    /// its recent samples.
    fn reset_relative(&self, i: usize) {
        self.mount[i].store(0, Relaxed);
        for sample in &self.recent {
            sample[i].store(0, Relaxed);
        }
    }

    /// The counters that changed in the last ten seconds: each with what
    /// it's gone up by in each half second since, newest first.
    pub fn recent_to_text(&self, out: &mut Printbuf) {
        let oldest = &self.recent[c::NR_RECENT_COUNTERS as usize - 1];

        for (i, ctr) in COUNTERS.iter().enumerate() {
            let mut prev = self.now(i);
            if oldest[i].load(Relaxed) == prev {
                continue;
            }

            prt!(out, "{}:", ctr.name);
            for sample in &self.recent {
                let s = sample[i].load(Relaxed);
                prt!(out, "\t{}", prev.wrapping_sub(s));
                prev = s;
            }
            prt!(out, "\n");
        }
    }
}

impl Fs {
    pub fn counter_read(&self, idx: usize) -> u64 {
        assert!(idx < NR);
        self.counters.now(idx)
    }

    /// Zero counter @idx - and what's relative to it, its count at mount and
    /// its recent samples, which would otherwise read as having gone backwards.
    pub fn counter_reset(&self, idx: usize) {
        if idx >= NR {
            return;
        }

        self.counters.set_now(idx, 0);
        self.counters.reset_relative(idx);
    }
}

impl SbFieldOps for c::bch_sb_field_counters {
    fn to_text(out: &mut Printbuf, _fs: Option<&Fs>, _sb: &c::bch_sb<Unvalidated>,
                f: &c::bch_sb_field_counters<Unvalidated>) {
        for ctr in COUNTERS {
            if let Some(v) = f.d.get(ctr.stable_id as usize) {
                prt!(out, "{} \t{}\n", ctr.name, v.get());
            }
        }
    }

    /// The counts now; those it has no slot for are left as they are.
    fn to_cpu(fs: &Fs, sb: &mut SbLockGuard<'_>) -> Result<(), BchError> {
        let d: &[Le64] = sb.field::<c::bch_sb_field_counters>().map_or(&[], |f| &f.d);
        for (i, ctr) in COUNTERS.iter().enumerate() {
            if let Some(v) = d.get(ctr.stable_id as usize) {
                fs.counters.set_now(i, v.get());
            }
        }
        Ok(())
    }

    /// The counters - memory's - with the field grown to a slot for every
    /// stable id if it can be, else as it is.
    fn from_cpu(fs: &Fs, guard: &mut SbLockGuard<'_>) -> Result<(), BchError> {
        let mut sb = fs.sb_resize(guard);

        let u64s = sb_field_u64s::<c::bch_sb_field_counters>(NR_STABLE);
        let _ = sb.field_get_minsize::<c::bch_sb_field_counters>(u64s);
        let Some(f) = sb.field_mut::<c::bch_sb_field_counters>() else { return Ok(()) };

        for (i, ctr) in COUNTERS.iter().enumerate() {
            if let Some(d) = f.d.get_mut(ctr.stable_id as usize) {
                *d = Le64::new(fs.counters.now(i));
            }
        }
        Ok(())
    }
}

crate::fs_delayed_work! {
    /// Every half second: the counters sampled into recent[], newest first.
    pub(crate) work RecentCounters: field counters_recent;

    fn run(fs: &Fs) {
        fs.counters.sample();
        Self::queue(fs);
    }
}

impl RecentCounters {
    pub(crate) fn queue(fs: &Fs) {
        fs.counters_recent.queue(Queue::system_dfl(), nsecs_to_jiffies(NSEC_PER_SEC / 2));
    }
}

/// The superblock's counters into memory, as both the counts now and at
/// mount. A counter the superblock has no slot for starts at 0.
pub fn counters_init(fs: &Fs) {
    let mut sb = fs.sb_lock();
    let _ = c::bch_sb_field_counters::to_cpu(fs, &mut sb);

    let d: &[Le64] = sb.field::<c::bch_sb_field_counters>().map_or(&[], |f| &f.d);
    for (i, ctr) in COUNTERS.iter().enumerate() {
        fs.counters.mount[i].store(d.get(ctr.stable_id as usize).map_or(0, |v| v.get()), Relaxed);
    }
}

// ── BCH_IOCTL_QUERY_COUNTERS ─────────────────────────────────────────────────

#[cfg(not(NO_BCACHEFS_CHARDEV))]
pub mod ioctl {
    use super::*;
    use crate::errcode::{Errcode, BchError};
    use crate::os::ioctl::UserVstruct;

    /// The counters, by stable id, up to the user's @nr of them: now, or at
    /// mount (BCH_IOCTL_QUERY_COUNTERS_MOUNT). @nr comes back as the number
    /// filled.
    pub fn query_counters(fs: &Fs, user: UserVstruct<c::bch_ioctl_query_counters>)
        -> Result<(), BchError>
    {
        let mut arg = user.head()?;

        let flags = u32::from(arg.flags);
        if flags & !c::BCH_IOCTL_QUERY_COUNTERS_MOUNT != 0 || arg.pad != 0 {
            return fs.throw(Errcode::EINVAL_ioctl_query_counters_bad_flags);
        }
        let mount = flags & c::BCH_IOCTL_QUERY_COUNTERS_MOUNT != 0;

        arg.nr = arg.nr.min(NR_STABLE as u16);
        user.set_head(&arg)?;

        let d = user.tail(&arg);
        for (i, ctr) in COUNTERS.iter().enumerate() {
            if ctr.stable_id < arg.nr {
                let v = if mount { fs.counters.mount(i) } else { fs.counters.now(i) };
                d.set(ctr.stable_id.into(), &v)?;
            }
        }
        Ok(())
    }
}
