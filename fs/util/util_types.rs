// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::DArray;
use crate::util::ffi::c_char;
use core::ffi::{c_int, c_uint, c_ulong, c_ulonglong, c_void};
use cstruct_macros::{CStruct, c_extern, c_typedef, TypeInfo};

c_typedef! {
    pub type bch_stacktrace = DArray<c_ulong>;
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
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
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
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
#[derive(Clone, Copy, Default, CStruct)]
pub struct memalloc_flags {
    pub flags: c_uint,
}

c_extern! {
    pub fn bch2_prt_datetime(out: *mut c::printbuf, sec: c::time64_t);
    pub fn bch2_prt_u64_base2_nbits(out: *mut c::printbuf, v: u64, nr_bits: c_uint);
    pub fn bch2_strtoull_h(cp: *const c_char, res: *mut c_ulonglong) -> c_int;
    pub fn bch2_read_flag_list(opt: *const c_char, list: *const *const c_char) -> u64;
    pub fn bch2_get_random_u64_below(ceil: u64) -> u64;
    pub fn rust_kzalloc_nofail(size: usize) -> *mut c_void;
    pub fn rust_kfree(p: *const c_void);
    pub fn rust_kvzalloc(size: usize) -> *mut c_void;
    pub fn rust_kvfree(p: *mut c_void);
    pub fn rust_kvfree_rcu(p: *mut c_void);
    pub fn bch2_local_clock() -> u64;
    pub fn bch2_cond_resched();
    pub fn bch2_msleep(ms: c_uint);
    pub fn bch2_fatal_signal_pending() -> bool;
    pub fn bch2_nsecs_to_jiffies(ns: u64) -> c_ulong;
    pub fn bch2_acc_u64s_percpu(acc: *mut u64, src: *const u64, nr: c_uint);
    pub fn bch2_bdev_rot(bdev: *mut c::block_device) -> bool;
    pub fn bch2_pr_time_units(out: *mut c::printbuf, ns: u64);
    pub fn bch2_prt_current_backtrace(out: *mut c::printbuf, skipnr: c_uint, gfp: c::gfp_t) -> c_int;
    pub fn bch2_get_random_u64() -> u64;
    pub fn bch2_ktime_get_coarse_real_ts64(ts: *mut c::timespec64);
    pub fn bch2_queue_work(wq: *mut c::workqueue_struct, work: *mut c::work_struct) -> bool;
    pub fn bch2_init_delayed_work(dwork: *mut c::delayed_work,
                                  func: Option<unsafe extern "C" fn(*mut c::work_struct)>);
    pub fn bch2_queue_delayed_work(wq: *mut c::workqueue_struct, dwork: *mut c::delayed_work,
                                   delay: c_ulong) -> bool;
    pub fn bch2_cancel_delayed_work_sync(dwork: *mut c::delayed_work) -> bool;
    pub fn bch2_system_long_wq() -> *mut c::workqueue_struct;
    pub fn bch2_capable(cap: c_int) -> bool;
    pub fn bch2_sysfs_create_file(kobj: *mut c::kobject, attr: *const c::attribute) -> c_int;
    #[cfg(__KERNEL__)]
    pub fn bch2_cpumask_local_spread(i: c_uint) -> c_uint;
    #[cfg(__KERNEL__)]
    pub fn bch2_nr_cpu_ids() -> c_uint;
}
