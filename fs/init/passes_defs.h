/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_INIT_PASSES_DEFS_H
#define _BCACHEFS_INIT_PASSES_DEFS_H

#include "enum_kind.h"

enum __enum_flags bch_run_recovery_pass_flags {
	RUN_RECOVERY_PASS_ratelimit	= BIT(0),
	/*
	 * Schedule in memory only, without taking sb_lock, so it's safe from
	 * contexts that hold btree locks (e.g. triggers): the schedule touches
	 * only in-memory recovery state and never writes the superblock. The
	 * need is re-derivable, so persistence isn't required.
	 */
	RUN_RECOVERY_PASS_ephemeral	= BIT(1),
	/*
	 * Don't schedule if the pass already completed successfully this
	 * instance: for callers that schedule cleanup passes on encountering
	 * damage those passes might not fix. If the pass ran and the damage is
	 * still here, rescheduling can't help - it just re-arms the pass in the
	 * superblock on every encounter, forcing fsck on every subsequent mount.
	 */
	RUN_RECOVERY_PASS_skip_if_complete = BIT(2),
};

struct sb_write;

#endif /* _BCACHEFS_INIT_PASSES_DEFS_H */
