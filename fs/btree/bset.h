/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_BSET_H
#define _BCACHEFS_BSET_H

#include <linux/kernel.h>
#include <linux/types.h>

#include "bcachefs.h"
#include "btree/bkey.h"
#include "btree/bkey_methods.h"
#include "btree/types.h"
#include "util/util.h" /* for time_stats */
#include "util/vstructs.h"

#include "btree/bset_types.h"

#define BSET_NO_AUX_TREE_VAL	(U16_MAX)
#define BSET_RW_AUX_TREE_VAL	(U16_MAX - 1)

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

/*
 * BSET_CACHELINE was originally intended to match the hardware cacheline size -
 * it used to be 64, but I realized the lookup code would touch slightly less
 * memory if it was 128.
 *
 * It definites the number of bytes (in struct bset) per struct bkey_float in
 * the auxiliar search tree - when we're done searching the bset_float tree we
 * have this many bytes left that we do a linear search over.
 *
 * Since (after level 5) every level of the bset_tree is on a new cacheline,
 * we're touching one fewer cacheline in the bset tree in exchange for one more
 * cacheline in the linear search - but the linear search might stop before it
 * gets to the second cacheline.
 */

#define BSET_CACHELINE		256

static inline size_t __btree_keys_cachelines(unsigned byte_order)
{
	return (1U << byte_order) / BSET_CACHELINE;
}

static inline size_t __btree_aux_data_bytes(unsigned byte_order)
{
	return __btree_keys_cachelines(byte_order) * 8;
}

static inline size_t btree_keys_cachelines(const struct btree *b)
{
	return __btree_keys_cachelines(b->byte_order);
}

static inline size_t btree_aux_data_bytes(const struct btree *b)
{
	return __btree_aux_data_bytes(b->byte_order);
}

static inline size_t btree_aux_data_u64s(const struct btree *b)
{
	return btree_aux_data_bytes(b) / sizeof(u64);
}

#define for_each_bset(_b, _t)						\
	for (struct bset_tree *_t = (_b)->set; _t < (_b)->set + (_b)->nsets; _t++)

#define for_each_bset_c(_b, _t)						\
	for (const struct bset_tree *_t = (_b)->set; _t < (_b)->set + (_b)->nsets; _t++)

#define bset_tree_for_each_key(_b, _t, _k)				\
	for (_k = btree_bkey_first(_b, _t);				\
	     _k != btree_bkey_last(_b, _t);				\
	     _k = bkey_p_next(_k))

static inline bool bset_has_ro_aux_tree(const struct bset_tree *t)
{
	return bset_aux_tree_type(t) == BSET_RO_AUX_TREE;
}

static inline bool bset_has_rw_aux_tree(struct bset_tree *t)
{
	return bset_aux_tree_type(t) == BSET_RW_AUX_TREE;
}

static inline void bch2_bset_set_no_aux_tree(struct btree *b,
					    struct bset_tree *t)
{
	BUG_ON(t < b->set);

	for (; t < b->set + ARRAY_SIZE(b->set); t++) {
		t->size = 0;
		t->extra = BSET_NO_AUX_TREE_VAL;
		t->aux_data_offset = U16_MAX;
	}
}

static inline void btree_node_set_format(struct btree *b,
					 struct bkey_format f)
{
	int len;

	b->format	= f;
	b->nr_key_bits	= bkey_format_key_bits(&f);

	bch2_compute_bkey_unpack_consts(b);

	len = bch2_compile_bkey_format(&b->format, b->aux_data);
	BUG_ON(len < 0 || len > U8_MAX);

	b->unpack_fn_len = len;

	bch2_bset_set_no_aux_tree(b, b->set);
}

static inline struct bset *bset_next_set(struct btree *b,
					 unsigned block_bytes)
{
	struct bset *i = btree_bset_last(b);

	EBUG_ON(!is_power_of_2(block_bytes));

	return ((void *) i) + round_up(vstruct_bytes(i), block_bytes);
}

void bch2_btree_keys_init(struct btree *);

void bch2_bset_init_first(struct btree *, struct bset *);
void bch2_bset_init_next(struct btree *, struct btree_node_entry *);
void bch2_bset_build_aux_tree(struct btree *, struct bset_tree *, bool);

void bch2_bset_insert(struct btree *, struct bkey_packed *, struct bkey_i *,
		      unsigned);
void bch2_bset_delete(struct btree *, struct bkey_packed *, unsigned);

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

struct bset_tree *bch2_bkey_to_bset(struct btree *, struct bkey_packed *);

struct bkey_packed *bch2_bkey_prev_filter(struct btree *, struct bset_tree *,
					  struct bkey_packed *, unsigned);

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

void bch2_btree_node_iter_push(struct btree_node_iter *, struct btree *,
			      const struct bkey_packed *,
			      const struct bkey_packed *);
void bch2_btree_node_iter_init(struct bch_fs *, struct btree *,
			       struct btree_node_iter *, struct bpos *);
void bch2_btree_node_iter_init_from_start(struct btree_node_iter *,
					  struct btree *);
struct bkey_packed *bch2_btree_node_iter_bset_pos(struct btree_node_iter *,
						 struct btree *,
						 struct bset_tree *);

void bch2_btree_node_iter_sort(struct btree_node_iter *, struct btree *);
void bch2_btree_node_iter_set_drop(struct btree_node_iter *,
				   struct btree_node_iter_set *);
void bch2_btree_node_iter_advance(struct btree_node_iter *, struct btree *);

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

struct bkey_packed *bch2_btree_node_iter_prev_all(struct btree_node_iter *,
						  struct btree *);
struct bkey_packed *bch2_btree_node_iter_prev(struct btree_node_iter *,
					      struct btree *);

struct bkey_s_c bch2_btree_node_iter_peek_unpack(struct btree_node_iter *,
						struct btree *,
						struct bkey *);

#define for_each_btree_node_key(b, k, iter)				\
	for (bch2_btree_node_iter_init_from_start((iter), (b));		\
	     (k = bch2_btree_node_iter_peek((iter), (b)));		\
	     bch2_btree_node_iter_advance(iter, b))

#define for_each_btree_node_key_unpack(b, k, iter, unpacked)		\
	for (bch2_btree_node_iter_init_from_start((iter), (b));		\
	     (k = bch2_btree_node_iter_peek_unpack((iter), (b), (unpacked))).k;\
	     bch2_btree_node_iter_advance(iter, b))

/* Accounting: */

struct btree_nr_keys bch2_btree_node_count_keys(struct btree *);

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

static inline void btree_keys_account_val_delta(struct btree *b,
						struct bkey_packed *k,
						int delta)
{
	struct bset_tree *t = bch2_bkey_to_bset(b, k);

	b->nr.live_u64s			+= delta;
	b->nr.bset_u64s[t - b->set]	+= delta;
}

#define btree_keys_account_key_add(_nr, _bset_idx, _k)		\
	btree_keys_account_key(_nr, _bset_idx, _k, 1)
#define btree_keys_account_key_drop(_nr, _bset_idx, _k)	\
	btree_keys_account_key(_nr, _bset_idx, _k, -1)

#define btree_account_key_add(_b, _k)				\
	btree_keys_account_key(&(_b)->nr,			\
		bch2_bkey_to_bset(_b, _k) - (_b)->set, _k, 1)
#define btree_account_key_drop(_b, _k)				\
	btree_keys_account_key(&(_b)->nr,			\
		bch2_bkey_to_bset(_b, _k) - (_b)->set, _k, -1)

void bch2_btree_keys_stats(const struct btree *, struct bset_stats *);
void bch2_bfloat_to_text(struct printbuf *, struct btree *,
			 struct bkey_packed *);

/* Debug stuff */

void bch2_btree_node_keys_to_text(struct printbuf *, struct bch_fs *, struct btree *);

void bch2_bset_to_text(struct printbuf *,
		       struct bch_fs *, struct btree *,
		       struct bset *, unsigned);
void bch2_dump_btree_node_iter(struct btree *, struct btree_node_iter *);

void __bch2_verify_btree_nr_keys(struct btree *);
void __bch2_btree_node_iter_verify(struct btree_node_iter *, struct btree *);

/*
 * Always runs the cheap structural checks in debug builds; the expensive ones
 * are behind debug_check_bset_lookups.
 */
static inline void bch2_btree_node_iter_verify(struct btree_node_iter *iter,
					       struct btree *b)
{
	if (IS_ENABLED(CONFIG_BCACHEFS_DEBUG))
		__bch2_btree_node_iter_verify(iter, b);
}

static inline void bch2_verify_btree_nr_keys(struct btree *b)
{
	if (IS_ENABLED(CONFIG_BCACHEFS_DEBUG) &&
	    static_branch_unlikely(&bch2_debug_check_btree_accounting))
		__bch2_verify_btree_nr_keys(b);
}

#endif /* _BCACHEFS_BSET_H */
