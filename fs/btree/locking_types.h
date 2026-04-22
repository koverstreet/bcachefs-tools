/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_BTREE_LOCKING_TYPES_H
#define _BCACHEFS_BTREE_LOCKING_TYPES_H

#include "util/darray.h"
#include "util/six.h"
#include "btree/types.h"

/* State used for the cycle detector */

/*
 * @trans wants to lock @b with type @type
 */
struct trans_waiting_for_lock {
	struct btree_trans		*trans;
	struct btree_bkey_cached_common	*node_want;
	enum six_lock_type		lock_want:8;

	/* for iterating over held locks :*/
	u8				level;
	btree_path_idx_t		path_idx;
	u16				waitlist_idx;
	bool				waitlist_idx_initialized;
	u64				lock_start_time;
};

struct lock_graph {
	struct trans_waiting_for_lock	g[8];
	unsigned			nr;
	bool				printed_chain;
};

#endif /* _BCACHEFS_BTREE_LOCKING_TYPES_H */
