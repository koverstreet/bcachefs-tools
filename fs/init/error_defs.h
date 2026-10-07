/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_INIT_ERROR_DEFS_H
#define _BCACHEFS_INIT_ERROR_DEFS_H

struct bch_dev;

struct bch_fs;

struct work_struct;

/*
 * Fsck errors: inconsistency errors we detect at mount time, and should ideally
 * be able to repair:
 */

struct fsck_err_state {
	enum bch_sb_error_id	id;
	u64			nr;
	bool			ratelimited;
	int			ret;
	int			fix;
	char			*last_msg;
};

enum bch_validate_flags;

#endif /* _BCACHEFS_INIT_ERROR_DEFS_H */
