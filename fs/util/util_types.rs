// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use core::ffi::{c_uint, c_ulong};
use cstruct_macros::{MaybeZeroable, TypeInfo};

#[repr(C)]
#[derive(Clone, Copy, Default, MaybeZeroable, TypeInfo)]
pub struct bch_ratelimit {
    /* Next time we want to do some work, in nanoseconds */
    pub next: u64,

    /*
     * Rate at which we want to do work, in units per nanosecond
     * The units here correspond to the units passed to
     * bch2_ratelimit_increment()
     */
    pub rate: c_uint,
}

#[repr(C)]
#[derive(Clone, Copy, Default, MaybeZeroable, TypeInfo)]
pub struct bch_pd_controller {
    pub rate: c::bch_ratelimit,
    pub last_update: c_ulong,

    pub last_actual: i64,
    pub smoothed_derivative: i64,

    pub p_term_inverse: c_uint,
    pub d_smooth: c_uint,
    pub d_term: c_uint,

    /* for exporting to sysfs (no effect on behavior) */
    pub last_derivative: i64,
    pub last_proportional: i64,
    pub last_change: i64,
    pub last_target: i64,

    /*
     * If true, the rate will not increase if bch2_ratelimit_delay()
     * is not being called often enough.
     */
    pub backpressure: bool,
}

#[repr(C)]
#[derive(Clone, Copy, Default, MaybeZeroable)]
pub struct memalloc_flags {
    pub flags: c_uint,
}

pub use crate::c::{
    rust_kzalloc_nofail, rust_kvzalloc_nofail, rust_kfree, rust_kvfree,
    rust_kvfree_rcu, rust_bio_pages_alloc, rust_bio_pages_free,
    rust_kfree_rcu_mightsleep,
    bch2_set_current_state, bch2_set_current_state_running,
};
