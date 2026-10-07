// SPDX-License-Identifier: GPL-2.0

//! The data types of util/time_stats_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::{CStruct, c_const, c_extern, c_verbatim};

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct time_unit {
    pub name: *const crate::util::ffi::c_char,
    pub nsecs: u64,
}
c_default!(time_unit);

c_const! {
    /*
     * quantiles - do not use:
     *
     * Only enabled if bch2_time_stats->quantiles_enabled has been manually set - don't
     * use in new code.
     */
    #[c_int]
    pub const NR_QUANTILES: u32 = 15;
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct quantile_entry {
    pub m: u64,
    pub step: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct quantiles {
    pub entries: [c::quantile_entry; c::NR_QUANTILES as usize],
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct time_stat_buffer_entry {
    pub start: u64,
    pub end: u64,
}

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct time_stat_buffer {
    /*
     * Protects nr/entries on the owning cpu: irq/preempt disable on
     * !PREEMPT_RT, a real per-cpu lock on RT - which is what makes
     * taking stats->lock (sleeping on RT) from the buffer-full flush
     * legal there. Cross-cpu readers (to_seq_buf, reset) don't take
     * it; they race the owner by design, bounded by nr.
     */
    pub lock: c::local_lock_t,
    pub nr: core::ffi::c_uint,
    pub entries: [c::time_stat_buffer_entry; 31],
}
c_default!(time_stat_buffer);

c_const! {
    /* default weight for streaming median+MAD: half-life ≈ 2^N samples */
    #[c_int]
    pub const TIME_STATS_MV_WEIGHT: u32 = 8;
}

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct bch2_time_stats {
    pub lock: c::spinlock_t,
    pub have_quantiles: bool,
    #[c("struct time_stat_buffer __percpu *buffer")]
    pub buffer: *mut c::time_stat_buffer,
    /* all fields are in nanoseconds */
    pub min_duration: u64,
    pub max_duration: u64,
    pub total_duration: u64,
    pub max_freq: u64,
    pub min_freq: u64,
    pub last_event: u64,
    pub last_event_start: u64,
    pub start_time: u64,

    pub duration_stats: c::mean_and_variance,
    pub freq_stats: c::mean_and_variance,

    pub duration_stats_weighted: c::mean_and_variance,
    pub freq_stats_weighted: c::mean_and_variance,
}
c_default!(bch2_time_stats);

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct bch2_time_stats_quantiles {
    pub stats: c::bch2_time_stats,
    pub quantiles: c::quantiles,
}
c_default!(bch2_time_stats_quantiles);

c_verbatim!(r#"
struct seq_buf;
"#);

// What Rust calls of util/time_stats.h: C gets these as prototypes, in util/time_stats_gen.h
// with the types - see c_extern! in types/cstruct.rs.
c_extern! {
    pub fn bch2_time_stats_to_json(arg1: *mut c::seq_buf, arg2: *mut c::bch2_time_stats, epoch_name: *const crate::util::ffi::c_char, flags: core::ffi::c_uint);
}
