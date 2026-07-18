/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_INIT_ERROR_TYPES_H
#define _BCACHEFS_INIT_ERROR_TYPES_H

#include "sb/errors_types.h"

struct fsck_err_state;

struct bch_fs_errors {
	DARRAY(struct fsck_err_state *)	msgs;
	struct mutex		msgs_lock;
	bool			msgs_alloc_err;

	bch_sb_errors_cpu	counts;
	struct mutex		counts_lock;
};

#endif /* _BCACHEFS_INIT_ERROR_TYPES_H */
