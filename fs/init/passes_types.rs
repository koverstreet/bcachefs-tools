// SPDX-License-Identifier: GPL-2.0

//! The data types of init/passes_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{DArray, c_default};
use cstruct_macros::{CStruct, c_extern};
use typeinfo_macros::TypeInfo;


#[repr(C)]
#[derive(CStruct, TypeInfo)]
pub struct bch_fs_recovery {
    /*
     * The running pass's progress, for recovery_status to read. One
     * indicator rather than a stack because no pass has two live at once -
     * which is what check_reconcile_work_data_btrees() exists to keep true.
     */
    pub progress: c::progress_indicator,

    /*
     * When @current_pass started, monotonic, for recovery_status to print
     * how long it's been going. A poller can time passes by watching
     * @current_pass change; whoever cats the sysfs file once can't.
     */
    pub pass_start_time: u64,

    /* counterpart to c->sb.recovery_passes_required */
    pub scheduled_passes_ephemeral: u64,

    pub current_passes: u64,
    pub current_pass: c::bch_recovery_pass,
    pub rewound_from: c::bch_recovery_pass,
    pub rewound_to: c::bch_recovery_pass,

    /* never rewinds version of curr_pass */
    pub pass_done: c::bch_recovery_pass,

    /* bitmask of recovery passes that we actually ran */
    pub passes_complete: u64,
    /*
     * Every pass this run dispatched, successful or not - the rewind
     * gate: we never rewind to (or re-queue behind us) a pass that
     * already ran this run. Gating on passes_complete alone looped: a
     * pass that runs and fails never completes, so under
     * errors=continue a later pass re-requesting it rewound forever
     * (delete_dead_snapshots <-> check_subvols on an unrepairable
     * snapshot/subvol edge).
     */
    pub passes_attempted: u64,
    pub passes_failing: u64,
    pub passes_ratelimiting: u64,

    /*
     * Cost-model retry ratelimit for the passes in passes_failing, kept in
     * memory only: a failing pass must not write the sb recovery_pass_entry,
     * but we still want the same last_run/last_runtime throttle so automatic
     * recovery doesn't hammer a pass that keeps failing.
     */
    pub passes_failing_ratelimit: [c::recovery_pass_entry; c::BCH_RECOVERY_PASS_NR as usize],
    /* Consecutive failures, for exponential backoff; zeroed on success */
    pub passes_failing_nr: [u8; c::BCH_RECOVERY_PASS_NR as usize],

    /*
     * Logged ops in the btree when we went rw - the ones from before this
     * mount, which recovery resumes. Positions in BTREE_ID_logged_ops,
     * ascending: see bch2_logged_ops_note_unfinished().
     */
    #[c("DARRAY(u64) logged_ops_unfinished")]
    pub logged_ops_unfinished: DArray<u64>,

    pub lock: c::spinlock_t,
    pub run_lock: c::mutex,
    pub work: c::work_struct,
}
c_default!(bch_fs_recovery);

// What Rust calls of init/passes.h: C gets these as prototypes, in init/passes_gen.h
// with the types - see c_extern! in types/cstruct.rs.
c_extern! {
    #[c("const char * const bch2_recovery_passes[]")]
    pub static bch2_recovery_passes: [*const crate::util::ffi::c_char; 0usize];
    pub fn bch2_recovery_passes_to_stable(v: u64) -> u64;
    pub fn bch2_recovery_passes_from_stable(v: u64) -> u64;
    pub fn bch2_recovery_pass_set_no_ratelimit(arg1: *mut c::bch_fs, arg2: c::bch_recovery_pass);
    pub fn bch2_run_explicit_recovery_pass(arg1: *mut c::bch_fs, arg2: *mut c::printbuf, arg3: c::bch_recovery_pass, arg4: c::bch_run_recovery_pass_flags) -> core::ffi::c_int;
    pub fn bch2_require_recovery_pass(arg1: *mut c::bch_fs, arg2: *mut c::printbuf, arg3: c::bch_recovery_pass) -> core::ffi::c_int;
}
