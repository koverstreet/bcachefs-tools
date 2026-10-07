/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_INIT_PROGRESS_TYPES_H
#define _BCACHEFS_INIT_PROGRESS_TYPES_H

/*
 * Lame progress indicators
 *
 * We don't like to use these because they print to the dmesg console, which is
 * spammy - we much prefer to be wired up to a userspace programm (e.g. via
 * thread_with_file) and have it print the progress indicator.
 *
 * But some code is old and doesn't support that, or runs in a context where
 * that's not yet practical (mount).
 */

struct progress_indicator {
	const char		*msg;
	enum bch_progress_units	units;
	struct bbpos		pos;
	unsigned long		next_print;
	u64			seen;
	u64			total;
	struct btree		*last_node;
	bool			silent;
};

#endif /* _BCACHEFS_INIT_PROGRESS_TYPES_H */
