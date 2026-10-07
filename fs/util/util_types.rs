// SPDX-License-Identifier: GPL-2.0

//! The data types of util/util_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::DArray;
use cstruct_macros::{CStruct, c_extern, c_verbatim};
use typeinfo_macros::TypeInfo;

c_verbatim!(r#"
struct closure;
"#);

c_verbatim!(r#"
DEFINE_DARRAY_NAMED(bch_stacktrace, unsigned long);
"#);

pub type bch_stacktrace = DArray<core::ffi::c_ulong>;

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
    pub rate: core::ffi::c_uint,
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_pd_controller {
    pub rate: c::bch_ratelimit,
    pub last_update: core::ffi::c_ulong,

    pub last_actual: i64,
    pub smoothed_derivative: i64,

    pub p_term_inverse: core::ffi::c_uint,
    pub d_smooth: core::ffi::c_uint,
    pub d_term: core::ffi::c_uint,

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
    pub flags: core::ffi::c_uint,
}

// What Rust calls of util/util.h: C gets these as prototypes, in util/util_gen.h
// with the types - see c_extern! in types/cstruct.rs.
c_extern! {
    pub fn bch2_prt_datetime(arg1: *mut c::printbuf, arg2: c::time64_t);
    pub fn bch2_strtoull_h(arg1: *const crate::util::ffi::c_char, arg2: *mut core::ffi::c_ulonglong) -> core::ffi::c_int;
    pub fn bch2_read_flag_list(arg1: *const crate::util::ffi::c_char, arg2: *const *const crate::util::ffi::c_char) -> u64;
    pub fn bch2_prt_task_backtrace(arg1: *mut c::printbuf, arg2: *mut c::task_struct, arg3: core::ffi::c_uint, arg4: c::gfp_t) -> core::ffi::c_int;
    pub fn bch2_get_random_u64_below(arg1: u64) -> u64;
    pub fn rust_kzalloc_nofail(size: usize) -> *mut core::ffi::c_void;
    pub fn rust_kfree(p: *const core::ffi::c_void);
    pub fn bch2_local_clock() -> u64;
    pub fn bch2_cond_resched();
    pub fn bch2_get_random_u64() -> u64;
    pub fn bch2_ktime_get_coarse_real_ts64(ts: *mut c::timespec64);
    pub fn bch2_queue_work(wq: *mut c::workqueue_struct, work: *mut c::work_struct) -> bool;
    pub fn bch2_capable(cap: core::ffi::c_int) -> bool;
    #[cfg(__KERNEL__)]
    pub fn bch2_cpumask_local_spread(i: core::ffi::c_uint) -> core::ffi::c_uint;
    #[cfg(__KERNEL__)]
    pub fn bch2_nr_cpu_ids() -> core::ffi::c_uint;
}
