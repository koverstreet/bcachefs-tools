/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_RECOVERY_PASSES_TYPES_H
#define _BCACHEFS_RECOVERY_PASSES_TYPES_H

#include "passes_format.h"

struct bch_fs_recovery {
	/* counterpart to c->sb.recovery_passes_required */
	u64			scheduled_passes_ephemeral;

	u64			current_passes;
	enum bch_recovery_pass	current_pass;
	enum bch_recovery_pass	rewound_from;
	enum bch_recovery_pass	rewound_to;

	/* never rewinds version of curr_pass */
	enum bch_recovery_pass	pass_done;

	/* bitmask of recovery passes that we actually ran */
	u64			passes_complete;
	/*
	 * Every pass this run dispatched, successful or not - the rewind
	 * gate: we never rewind to (or re-queue behind us) a pass that
	 * already ran this run. Gating on passes_complete alone looped: a
	 * pass that runs and fails never completes, so under
	 * errors=continue a later pass re-requesting it rewound forever
	 * (delete_dead_snapshots <-> check_subvols on an unrepairable
	 * snapshot/subvol edge).
	 */
	u64			passes_attempted;
	u64			passes_failing;
	u64			passes_ratelimiting;

	/*
	 * Cost-model retry ratelimit for the passes in passes_failing, kept in
	 * memory only: a failing pass must not write the sb recovery_pass_entry,
	 * but we still want the same last_run/last_runtime throttle so automatic
	 * recovery doesn't hammer a pass that keeps failing.
	 */
	struct recovery_pass_entry passes_failing_ratelimit[BCH_RECOVERY_PASS_NR];

	spinlock_t		lock;
	struct mutex		run_lock;
	struct work_struct	work;
};

#endif /* _BCACHEFS_RECOVERY_PASSES_TYPES_H */
