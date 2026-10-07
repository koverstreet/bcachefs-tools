// SPDX-License-Identifier: GPL-2.0

//! The data types of data/reconcile/types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::CStruct;
use typeinfo_macros::TypeInfo;


#[repr(C)]
#[derive(Clone, Copy, CStruct, TypeInfo)]
pub struct bch_fs_reconcile {
    #[c("struct task_struct __rcu *thread")]
    pub thread: *mut c::task_struct,
    /*
     * @thread being set means we started one and haven't stopped it, not
     * that it's running: it also exits on its own if do_reconcile() fails.
     * Nonzero iff it did.
     */
    pub thread_exit_ret: core::ffi::c_int,
    pub kick: u32,

    pub running: bool,
    pub wait_iotime_start: u64,
    pub wait_iotime_end: u64,
    pub wait_wallclock_start: u64,

    pub phase: core::ffi::c_uint,
    pub work_pos: c::bbpos,
    pub work_stats: c::bch_move_stats,
    pub progress: c::progress_indicator,

    pub scan_start: c::bbpos,
    pub scan_end: c::bbpos,
    pub scan_stats: c::bch_move_stats,

    /* In-flight opt changes - see bch2_set_reconcile_needs_scan_pre/post() */
    pub scans_in_flight: c::rhashtable,
    pub scans_in_flight_init_done: bool,
    pub scans_in_flight_lock: c::mutex,

    /*
     * Stripes whose repair failed in a way only a change elsewhere can fix
     * - no space, too few usable devices: the scan skips them, so reconcile
     * carries on with everything else and goes idle once they're all
     * that's left, instead of retrying them back to back. Stripes have no
     * on-disk pending state, as extents do; in memory, a remount retries
     * each once. Owned by the reconcile thread.
     */
    pub stripes_pending: c::cuckoo_u64,
    pub stripes_pending_copygc_run_count: u32,

    pub on_battery: bool,
    #[cfg(CONFIG_POWER_SUPPLY)]
    pub power_notifier: c::notifier_block,
}
c_default!(bch_fs_reconcile);
