/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_COPYGC_TYPES_H
#define _BCACHEFS_COPYGC_TYPES_H

#include "init/dev_types.h"

struct bch_fs_copygc {
	struct task_struct __rcu *thread;
	struct write_point	write_point;
	s64			wait_at;
	s64			wait;
	bool			running;
	bool			pressure_pending;
	bool			current_run_pressure;
	bool			last_run_pressure;
	u32			pressure_run_count;
	u32			run_count;
	u32			kick_count;
	wait_queue_head_t	running_wq;

	/*
	 * Devices over their fragmentation allowance, i.e. that copygc is trying
	 * to free space on. Set by copygc_dev_list() each pass, read unlocked by
	 * allocators that only need it to be roughly right.
	 *
	 * Not cleared when copygc is disabled: the devices are still full.
	 */
	struct bch_devs_mask	wants_space;

	/*
	 * Devices getting full, on a looser threshold than wants_space above -
	 * a superset of it, same lifetime and locking. Read by EC stripe reuse;
	 * see EC_REUSE_FREE_THRESHOLD_PCT.
	 */
	struct bch_devs_mask	low_on_space;

	/* Dedicated workqueue for btree updates: */
	struct workqueue_struct	*wq;
};

#endif /* _BCACHEFS_COPYGC_TYPES_H */
