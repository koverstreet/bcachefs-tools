/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_DATA_RECONCILE_WORK_TYPES_H
#define _BCACHEFS_DATA_RECONCILE_WORK_TYPES_H

#include "enum_kind.h"

#define RECONCILE_SCAN_TYPES()		\
	x(fs)				\
	x(metadata)			\
	x(pending)			\
	x(stripes)			\
	x(device)			\
	x(inum)

struct reconcile_scan {
	enum __enum_closed reconcile_scan_type {
#define x(t)	RECONCILE_SCAN_##t,
		RECONCILE_SCAN_TYPES()
#undef x
	}			type;

	union {
		unsigned	dev;
		u64		inum;
	};
};

/* No opt change touches more than one bracketed reconcile scan today: */
#define BCH_OPT_CHANGE_SCANS_MAX	4

/*
 * The reconcile-scan cookies an opt change registered as in-flight - see
 * bch2_set_reconcile_needs_scan_pre(). Constructed empty, populated by
 * bch2_opt_hook_pre_set(); the destructor unregisters them, so an opt change
 * that errors out (or never reaches bch2_opt_hook_post_set()) doesn't leak a
 * registration and wedge the reconcile thread on that cookie.
 */
struct opt_change_scope {
	struct bch_fs		*c;
	unsigned		nr;
	u64			cookies[BCH_OPT_CHANGE_SCANS_MAX];
};

#endif /* _BCACHEFS_DATA_RECONCILE_WORK_TYPES_H */
