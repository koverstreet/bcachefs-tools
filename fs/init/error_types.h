/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_INIT_ERROR_TYPES_H
#define _BCACHEFS_INIT_ERROR_TYPES_H

#include "sb/errors_types.h"

struct fsck_err_state;

/*
 * A path that fsck damaged or couldn't fully recover, remembered so we can
 * print a short per-path summary at the end instead of burying it in the error
 * firehose. Keyed by (inum, snapshot); reasons is a bitmask of enum
 * bch_fsck_damage_type - deduped and OR'd on insert, so one line per path lists
 * everything that happened to it, and re-recording on transaction restart is a
 * no-op.
 */
struct fsck_damaged_path {
	u64			inum;
	u32			snapshot;
	u32			reasons;
};

struct bch_fs_errors {
	DARRAY(struct fsck_err_state *)	msgs;
	struct mutex		msgs_lock;
	bool			msgs_alloc_err;

	DARRAY(struct fsck_damaged_path) damaged_paths;
	bool			damaged_paths_alloc_err;

	bch_sb_errors_cpu	counts;
	struct mutex		counts_lock;
};

#endif /* _BCACHEFS_INIT_ERROR_TYPES_H */
