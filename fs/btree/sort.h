/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_BKEY_SORT_H
#define _BCACHEFS_BKEY_SORT_H

#include "btree/interior.h"

#include "btree/node/sort_gen.h"

static inline void sort_iter_init(struct sort_iter *iter, struct btree *b, unsigned size)
{
	iter->b = b;
	iter->used = 0;
	iter->size = size;
}

static inline void sort_iter_stack_init(struct sort_iter_stack *iter, struct btree *b)
{
	sort_iter_init(&iter->iter, b, ARRAY_SIZE(iter->sets));
}

/*
 * If we have MAX_BSETS (3) bsets, should we sort them all down to just one?
 *
 * The first bset is going to be of similar order to the size of the node, the
 * last bset is bounded by btree_write_set_buffer(), which is set to keep the
 * memmove on insert from being too expensive: the middle bset should, ideally,
 * be the geometric mean of the first and the last.
 *
 * Returns true if the middle bset is greater than that geometric mean:
 */
static inline bool should_compact_all(struct bch_fs *c, struct btree *b)
{
	unsigned mid_u64s_bits =
		(ilog2(btree_max_u64s(c)) + BTREE_WRITE_SET_U64s_BITS) / 2;

	return bset_u64s(&b->set[1]) > 1U << mid_u64s_bits;
}

#endif /* _BCACHEFS_BKEY_SORT_H */
