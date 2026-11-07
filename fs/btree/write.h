/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_BTREE_WRITE_H
#define _BCACHEFS_BTREE_WRITE_H

#include "data/write_gen.h"

#include "btree/write_gen.h"

static inline void btree_node_write_if_need(struct btree_trans *trans, struct btree *b,
					    enum six_lock_type lock_held)
{
	bch2_btree_node_write_trans(trans, b, lock_held, BTREE_WRITE_only_if_need);
}

#endif /* _BCACHEFS_BTREE_WRITE_H */
