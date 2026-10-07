/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_ALLOC_BACKPOINTERS_TYPES_H
#define _BCACHEFS_ALLOC_BACKPOINTERS_TYPES_H

struct wb_maybe_flush;

DEFINE_DARRAY_NAMED(darray_bkey_i_backpointer, struct bkey_i_backpointer);

struct progress_indicator;

struct bp_scan_iter {
	/* BTREE_ID_backpointers, BTREE_ID_stripe_backpointers */
	enum btree_id			btree;
	struct bpos			pos;
	u64				nr_flushes;
	struct progress_indicator	*progress;
	darray_bkey_i_backpointer	bps;
};

#endif /* _BCACHEFS_ALLOC_BACKPOINTERS_TYPES_H */
