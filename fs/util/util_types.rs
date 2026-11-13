// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::util::ffi::c_char;
use core::ffi::{c_int, c_uint, c_ulong, c_void};
use cstruct_macros::{CStruct, c_const, c_extern, TypeInfo};

c_const! {
    /* alloc_workqueue()'s WQ_* flags, for util/workqueue.rs: util.c checks them */
    #[c_int]
    pub const BCH_WQ_UNBOUND: u32 = 1 << 1;
    #[c_int]
    pub const BCH_WQ_FREEZABLE: u32 = 1 << 2;
    #[c_int]
    pub const BCH_WQ_MEM_RECLAIM: u32 = 1 << 3;
    #[c_int]
    pub const BCH_WQ_HIGHPRI: u32 = 1 << 4;
    #[c_int]
    pub const BCH_WQ_CPU_INTENSIVE: u32 = 1 << 5;
    #[c_int]
    pub const BCH_WQ_PERCPU: u32 = 1 << 8;
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
    pub fn bch2_debugfs_create_file(name: *const c_char, mode: c::umode_t, parent: *mut c::dentry,
                                    data: *mut c_void, fops: *const c::file_operations) -> *mut c::dentry;
    pub fn rust_kzalloc_nofail(size: usize) -> *mut c_void;
    pub fn rust_kvzalloc_nofail(size: usize) -> *mut c_void;
    pub fn rust_kfree(p: *const c_void);
    pub fn rust_kvfree(p: *mut c_void);
    pub fn rust_kvfree_rcu(p: *mut c_void, head: *mut c::rcu_head);
    pub fn mempool_kvmalloc(gfp_mask: c::gfp_t, pool_data: *mut c_void) -> *mut c_void;
    pub fn mempool_kvfree(element: *mut c_void, pool_data: *mut c_void);
    pub fn rust_bio_pages_alloc(size: usize, gfp: c::gfp_t) -> *mut c_void;
    pub fn rust_bio_pages_free(p: *mut c_void, size: usize);
    pub fn rust_kfree_rcu_mightsleep(p: *mut c_void);
    pub fn bch2_cond_resched();
    pub fn bch2_fatal_signal_pending() -> bool;
    pub fn bch2_num_online_cpus() -> c_uint;
    pub fn bch2_bdev_rot(bdev: *mut c::block_device) -> bool;
    pub fn bch2_init_delayed_work(dwork: *mut c::delayed_work,
                                  func: Option<unsafe extern "C" fn(*mut c::work_struct)>);
    pub fn bch2_alloc_workqueue(name: *const c_char, flags: c_uint, max_active: c_int) -> *mut c::workqueue_struct;
    pub fn bch2_kthread_create(func: Option<unsafe extern "C" fn(*mut c_void) -> c_int>, data: *mut c_void,
                               name: *const c_char) -> *mut c::task_struct;
    pub fn bch2_kthread_should_stop() -> bool;
    pub fn bch2_current() -> *mut c::task_struct;
    pub fn bch2_set_current_state(state: c_uint);
    pub fn bch2_set_current_state_running();
    pub fn bch2_signal_pending_state(state: c_uint) -> bool;
    pub fn bch2_need_resched() -> bool;
    #[cfg(CONFIG_SMP)]
    pub fn bch2_owner_on_cpu(owner: *mut c::task_struct) -> bool;
    #[cfg(CONFIG_SMP)]
    pub fn bch2_rt_or_dl_task_current() -> bool;
    pub fn bch2_init_work(work: *mut c::work_struct, func: Option<unsafe extern "C" fn(*mut c::work_struct)>);
    pub fn bch2_set_worker_desc(desc: *const c_char);
    pub fn bch2_capable(cap: c_int) -> bool;
    #[cfg(__KERNEL__)]
    pub fn bch2_nr_cpu_ids() -> c_uint;
}
