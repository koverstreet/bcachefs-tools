// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::locking::Lock;
use crate::new_lock;
use crate::published;
use crate::types::c_default;
use crate::util::projections::Volatile;
use cstruct_macros::{CStruct, Init, TypeInfo};
use pin_init::pin_data;

/// What the runner keeps under its lock - taken with interrupts off, as
/// scheduling can come from where they are. Nothing else reads it.
#[repr(C)]
#[derive(Clone, Copy, CStruct, TypeInfo)]
pub struct bch_recovery_runner {
    /*
     * When current_pass started, monotonic, for recovery_status to print
     * how long it's been going. A poller can time passes by watching
     * current_pass change; whoever cats the sysfs file once can't.
     */
    pub pass_start_time: u64,
    pub rewound_from: c::bch_recovery_pass,
    pub rewound_to: c::bch_recovery_pass,
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
}
// rewound_from and rewound_to zeroed are pass 0, recovery_pass_empty: none
c_default!(bch_recovery_runner);

published! {
    /// What the runner publishes: read without its lock - by C, by the
    /// allocator, a word at a time - and written by the runner under it.
    #[derive(::pin_init::Zeroable)]
    pub struct RecoveryWords {
        /* counterpart to c->sb.recovery_passes_required */
        pub scheduled_passes_ephemeral: Volatile<u64>,
        pub current_passes: Volatile<u64>,
        pub current_pass: Volatile<c::bch_recovery_pass>,
        /* never rewinds version of curr_pass */
        pub pass_done: Volatile<c::bch_recovery_pass>,
        /* bitmask of recovery passes that we actually ran */
        pub passes_complete: Volatile<u64>,
    }
}

#[pin_data]
#[repr(C)]
#[derive(CStruct, TypeInfo, Init)]
pub struct bch_fs_recovery {
    /*
     * The running pass's progress, for recovery_status to read. One
     * indicator rather than a stack because no pass has two live at once -
     * which is what check_reconcile_work_data_btrees() exists to keep true.
     */
    pub progress: c::progress_indicator,

    /* the runner's state, under its lock, and what it publishes */
    #[pin]
    #[init(new_lock!(Default::default(), ::pin_init::zeroed(), "&c->recovery.lock"))]
    #[c_anon("spinlock_t lock;
	struct bch_recovery_runner runner;
	u64 scheduled_passes_ephemeral;
	u64 current_passes;
	enum bch_recovery_pass current_pass;
	enum bch_recovery_pass pass_done;
	u64 passes_complete")]
    pub runner: Lock<c::spinlock_t, bch_recovery_runner, RecoveryWords>,

    /* passes run under it - and what runs as a pass, it serializes */
    #[pin]
    #[init(new_lock!((), "&c->recovery.run_lock"))]
    #[c("struct mutex run_lock")]
    pub run_lock: Lock<c::mutex>,

    /* the async runner: run_async() */
    #[pin]
    #[init(crate::init::passes::async_work_init())]
    #[c("struct work_struct work")]
    pub work: crate::fs::FsWork,
}
