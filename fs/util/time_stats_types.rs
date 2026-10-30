// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::locking::Lock;
use crate::new_lock;
use crate::types::c_default;
use crate::util::ffi::Opaque;
use crate::util::percpu::CpuShared;
use crate::util::projections::Volatile;
use core::ffi::c_uint;
use core::sync::atomic::AtomicUsize;
use cstruct_macros::{CStruct, Init, c_const};
use pin_init::pin_data;

c_const! {
    /* a bch2_time_stats_quantiles' - a device's IO latency */
    #[c_int]
    pub const NR_QUANTILES: u32 = 15;
}

/* written under the stats' lock, read without it - write.c's congestion
 * accounting, the device's congested_to_text() */
#[repr(C)]
#[derive(CStruct)]
pub struct quantile_entry {
    pub m: Volatile<u64>,
    pub step: Volatile<u64>,
}

#[repr(C)]
#[derive(CStruct)]
pub struct quantiles {
    pub entries: [c::quantile_entry; c::NR_QUANTILES as usize],
}

#[repr(C)]
#[derive(CStruct)]
pub struct time_stat_buffer_entry {
    pub start: Volatile<u64>,
    pub end: Volatile<u64>,
}

#[repr(C)]
#[derive(CStruct)]
pub struct time_stat_buffer {
    /*
     * Guards nr and entries for its own cpu: interrupts off - or on
     * PREEMPT_RT a sleeping per-cpu lock, which is what lets the
     * buffer-full flush take the stats' lock under it there. Printing and
     * reset empty other cpus' buffers without it, racing their owners by
     * design, bounded by nr: so nr and the entries are Volatile, and the
     * lock, which its lockers change, Opaque.
     */
    #[c("local_lock_t lock")]
    pub lock: Opaque<c::local_lock_t>,
    pub nr: Volatile<c_uint>,
    pub entries: [c::time_stat_buffer_entry; 31],
}

// SAFETY: Volatiles, and the lock Opaque
unsafe impl CpuShared for time_stat_buffer_entry {}
unsafe impl CpuShared for time_stat_buffer {}

c_const! {
    /* default weight for streaming median+MAD: half-life ≈ 2^N samples */
    #[c_int]
    pub const TIME_STATS_MV_WEIGHT: u32 = 8;
}

/* the stats' lock's: all in nanoseconds */
#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct TimeStatsLocked {
    pub min_duration: u64,
    pub max_duration: u64,
    pub total_duration: u64,
    pub max_freq: u64,
    pub min_freq: u64,
    pub last_event: u64,

    pub duration_stats: c::mean_and_variance,
    pub freq_stats: c::mean_and_variance,

    pub duration_stats_weighted: c::mean_and_variance,
    pub freq_stats_weighted: c::mean_and_variance,
}
c_default!(TimeStatsLocked);

#[pin_data(PinnedDrop)]
#[repr(C)]
#[derive(CStruct, Init)]
pub struct bch2_time_stats {
    #[pin]
    #[init(new_lock!(c::TimeStatsLocked::new(), "&stats->lock"))]
    #[c_lock("spinlock_t lock")]
    pub lock: Lock<c::spinlock_t, TimeStatsLocked>,
    /* the per-cpu buffer, once events come fast enough: installed under the
     * lock, read without it - released and acquired, for what's in it - and
     * freed when the stats drop */
    #[c("struct time_stat_buffer __percpu *buffer")]
    pub buffer: AtomicUsize,
    /* a state change event in progress since: track_event_change()'s, unlocked */
    pub last_event_start: Volatile<u64>,
}

#[pin_data]
#[repr(C)]
#[derive(CStruct, Init)]
pub struct bch2_time_stats_quantiles {
    #[pin]
    #[init(c::bch2_time_stats::init())]
    pub stats: c::bch2_time_stats,
    pub quantiles: c::quantiles,
}
