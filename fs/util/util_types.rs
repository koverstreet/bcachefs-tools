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
    bch2_debugfs_create_file, rust_kzalloc_nofail, rust_kvzalloc_nofail, rust_kfree, rust_kvfree,
    rust_kvfree_rcu, rust_bio_pages_alloc, rust_bio_pages_free,
    rust_kfree_rcu_mightsleep, bch2_cond_resched, bch2_fatal_signal_pending, bch2_num_online_cpus,
    bch2_bdev_rot, bch2_kthread_create,
    bch2_kthread_should_stop, bch2_current, bch2_set_current_state, bch2_set_current_state_running,
    bch2_signal_pending_state, bch2_need_resched, bch2_capable,
};
#[cfg(CONFIG_SMP)]
pub use crate::c::{bch2_owner_on_cpu, bch2_rt_or_dl_task_current};
#[cfg(__KERNEL__)]
pub use crate::c::bch2_nr_cpu_ids;
