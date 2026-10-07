/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_DATA_RECONCILE_TRIGGER_TYPES_H
#define _BCACHEFS_DATA_RECONCILE_TRIGGER_TYPES_H

#include "enum_kind.h"

enum __enum_closed set_needs_reconcile_ctx {
	SET_NEEDS_RECONCILE_opt_change,
	SET_NEEDS_RECONCILE_opt_change_indirect,
	SET_NEEDS_RECONCILE_foreground,
	SET_NEEDS_RECONCILE_other,
};

/* Inodes in different snapshots may have different IO options: */
struct snapshot_io_opts_entry {
	u32			snapshot;
	struct bch_inode_opts	io_opts;
};

struct per_snapshot_io_opts {
	u64			cur_inum;
	bool			metadata;
	bool			fs_scan_cookie;
	bool			inum_scan_cookie;
	struct bch_devs_mask	dev_cookie;

	struct bch_inode_opts	fs_io_opts;
	DARRAY(struct snapshot_io_opts_entry) d;
};

#endif /* _BCACHEFS_DATA_RECONCILE_TRIGGER_TYPES_H */
