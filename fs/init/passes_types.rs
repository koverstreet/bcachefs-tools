// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::{CStruct, TypeInfo};

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

    pub lock: c::spinlock_t,
    pub run_lock: c::mutex,
    /* the async runner: bch2_run_async_recovery_passes() */
    #[c("struct work_struct work")]
    pub work: crate::fs::FsWork,
}
c_default!(bch_fs_recovery);
