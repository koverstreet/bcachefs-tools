/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_BSET_H
#define _BCACHEFS_BSET_H

#include <linux/kernel.h>
#include <linux/types.h>

#include "bcachefs.h"
#include "btree/bkey.h"
#include "btree/bkey/ops_gen.h"
#include "btree/trigger_gen.h"
#include "btree/node/bset_gen.h"
#include "btree/node/iter_gen.h"
#include "btree/path_gen.h"
#include "btree/types_gen.h"
#include "btree/node_gen.h"
#include "util/util.h" /* for time_stats */
#include "util/vstructs.h"

static inline enum bset_aux_tree_type bset_aux_tree_type(const struct bset_tree *t)
{
	switch (t->extra) {
	case BSET_NO_AUX_TREE_VAL:
		EBUG_ON(t->size);
		return BSET_NO_AUX_TREE;
	case BSET_RW_AUX_TREE_VAL:
		EBUG_ON(!t->size);
		return BSET_RW_AUX_TREE;
	default:
		EBUG_ON(!t->size);
		return BSET_RO_AUX_TREE;
	}
}

static inline size_t __btree_keys_cachelines(unsigned byte_order)
{
	return (1U << byte_order) / BSET_CACHELINE;
}

static inline size_t __btree_aux_data_bytes(unsigned byte_order)
{
	return __btree_keys_cachelines(byte_order) * 8;
}

static inline size_t btree_aux_data_bytes(const struct btree *b)
{
	return __btree_aux_data_bytes(b->byte_order);
}

#define for_each_bset(_b, _t)						\
	for (struct bset_tree *_t = (_b)->set; _t < (_b)->set + (_b)->nsets; _t++)

#define for_each_bset_c(_b, _t)						\
	for (const struct bset_tree *_t = (_b)->set; _t < (_b)->set + (_b)->nsets; _t++)

#define bset_tree_for_each_key(_b, _t, _k)				\
	for (_k = btree_bkey_first(_b, _t);				\
	     _k != btree_bkey_last(_b, _t);				\
	     _k = bkey_p_next(_k))

/* Bkey utility code */

static inline struct bset_tree *
bch2_bkey_to_bset_inlined(struct btree *b, struct bkey_packed *k)
{
	unsigned offset = __btree_node_key_to_offset(b, k);

	for_each_bset(b, t)
		if (offset <= t->end_offset) {
			EBUG_ON(offset < btree_bkey_first_offset(t));
			return t;
		}

	BUG();
}

static inline struct bkey_packed *
bch2_bkey_prev_all(struct btree *b, struct bset_tree *t, struct bkey_packed *k)
{
	return bch2_bkey_prev_filter(b, t, k, 0);
}

static inline struct bkey_packed *
bch2_bkey_prev(struct btree *b, struct bset_tree *t, struct bkey_packed *k)
{
	return bch2_bkey_prev_filter(b, t, k, 1);
}

/* Btree key iteration */

#define btree_node_iter_for_each(_iter, _set)				\
	for (struct btree_node_iter_set *_set = (_iter)->data;		\
	     _set < (_iter)->data + ARRAY_SIZE((_iter)->data) &&	\
	     (_set)->k != (_set)->end;					\
	     _set++)

static inline struct btree_node_iter_set *
btree_node_iter_set_find(struct btree_node_iter *iter, unsigned end_offset)
{
	btree_node_iter_for_each(iter, set)
		if (set->end == end_offset)
			return set;
	return NULL;
}

static inline bool __btree_node_iter_set_end(struct btree_node_iter *iter,
					     unsigned i)
{
	return iter->data[i].k == iter->data[i].end;
}

static inline bool bch2_btree_node_iter_end(struct btree_node_iter *iter)
{
	return __btree_node_iter_set_end(iter, 0);
}

/*
 * When keys compare equal, deleted keys compare first:
 *
 * XXX: only need to compare pointers for keys that are both within a
 * btree_node_iterator - we need to break ties for prev() to work correctly
 */
static inline int bkey_iter_cmp(const struct btree *b,
				const struct bkey_packed *l,
				const struct bkey_packed *r)
{
	return bch2_bkey_cmp_packed(b, l, r)
		?: (int) bkey_deleted(r) - (int) bkey_deleted(l)
		?: cmp_int(l, r);
}

static inline int btree_node_iter_cmp(const struct btree *b,
				      struct btree_node_iter_set l,
				      struct btree_node_iter_set r)
{
	return bkey_iter_cmp(b,
			__btree_node_offset_to_key(b, l.k),
			__btree_node_offset_to_key(b, r.k));
}

/* These assume r (the search key) is not a deleted key: */
static inline int bkey_iter_pos_cmp(const struct btree *b,
			const struct bkey_packed *l,
			const struct bpos *r)
{
	return bkey_cmp_left_packed(b, l, r)
		?: -((int) bkey_deleted(l));
}

static inline struct bkey_packed *
__bch2_btree_node_iter_peek_all(struct btree_node_iter *iter,
				struct btree *b)
{
	return __btree_node_offset_to_key(b, iter->data->k);
}

static inline struct bkey_packed *
bch2_btree_node_iter_peek_all(struct btree_node_iter *iter, struct btree *b)
{
	return !bch2_btree_node_iter_end(iter)
		? __btree_node_offset_to_key(b, iter->data->k)
		: NULL;
}

static inline struct bkey_packed *
bch2_btree_node_iter_peek(struct btree_node_iter *iter, struct btree *b)
{
	struct bkey_packed *k;

	while ((k = bch2_btree_node_iter_peek_all(iter, b)) &&
	       bkey_deleted(k))
		bch2_btree_node_iter_advance(iter, b);

	return k;
}

static inline struct bkey_packed *
bch2_btree_node_iter_next_all(struct btree_node_iter *iter, struct btree *b)
{
	struct bkey_packed *ret = bch2_btree_node_iter_peek_all(iter, b);

	if (ret)
		bch2_btree_node_iter_advance(iter, b);

	return ret;
}

#define for_each_btree_node_key(b, k, iter)				\
	for (bch2_btree_node_iter_init_from_start((iter), (b));		\
	     (k = bch2_btree_node_iter_peek((iter), (b)));		\
	     bch2_btree_node_iter_advance(iter, b))

/* Accounting: */

static inline void btree_keys_account_key(struct btree_nr_keys *n,
					  unsigned bset,
					  struct bkey_packed *k,
					  int sign)
{
	n->live_u64s		+= k->u64s * sign;
	n->bset_u64s[bset]	+= k->u64s * sign;

	if (bkey_packed(k))
		n->packed_keys	+= sign;
	else
		n->unpacked_keys += sign;
}

#define btree_keys_account_key_add(_nr, _bset_idx, _k)		\
	btree_keys_account_key(_nr, _bset_idx, _k, 1)

#define btree_account_key_add(_b, _k)				\
	btree_keys_account_key(&(_b)->nr,			\
		bch2_bkey_to_bset(_b, _k) - (_b)->set, _k, 1)
#define btree_account_key_drop(_b, _k)				\
	btree_keys_account_key(&(_b)->nr,			\
		bch2_bkey_to_bset(_b, _k) - (_b)->set, _k, -1)

/* Debug stuff */

#endif /* _BCACHEFS_BSET_H */
