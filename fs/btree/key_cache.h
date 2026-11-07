/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_BTREE_KEY_CACHE_H
#define _BCACHEFS_BTREE_KEY_CACHE_H

#include "btree/bkey.h"

int bch2_btree_key_cache_journal_flush(struct journal *,
				struct journal_entry_pin *, u64);
int bch2_btree_key_cache_flush_going_ro(struct bch_fs *);

static inline int bch2_btree_key_cache_cmp_fn(struct rhashtable_compare_arg *arg,
					      const void *obj)
{
	const struct bkey_cached *ck = obj;
	const struct bkey_cached_key *key = arg->key;

	return ck->key.btree_id != key->btree_id ||
		!bpos_eq(ck->key.pos, key->pos);
}

static const struct rhashtable_params bch2_btree_key_cache_params = {
	.head_offset		= offsetof(struct bkey_cached, hash),
	.key_offset		= offsetof(struct bkey_cached, key),
	.key_len		= sizeof(struct bkey_cached_key),
	.obj_cmpfn		= bch2_btree_key_cache_cmp_fn,
	.automatic_shrinking	= true,
};

struct bkey_cached *bch2_btree_key_cache_find(struct bch_fs *, enum btree_id, struct bpos);

int bch2_btree_path_traverse_cached(struct btree_trans *, btree_path_idx_t, unsigned);

void bch2_btree_insert_key_cached(struct btree_trans *, struct btree_insert_entry *);
void bch2_btree_key_cache_drop(struct btree_trans *,
			       struct btree_path *);

void bch2_fs_btree_key_cache_exit(struct bch_fs_btree_key_cache *);
int bch2_fs_btree_key_cache_init(struct bch_fs_btree_key_cache *);

void bch2_btree_key_cache_to_text(struct printbuf *, struct bch_fs_btree_key_cache *);

void bch2_btree_key_cache_exit(void);
int __init bch2_btree_key_cache_init(void);

#endif /* _BCACHEFS_BTREE_KEY_CACHE_H */
