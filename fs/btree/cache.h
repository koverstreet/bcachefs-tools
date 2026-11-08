/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_BTREE_CACHE_H
#define _BCACHEFS_BTREE_CACHE_H

#include "bcachefs.h"
#include "btree/node/bset_gen.h"
#include "btree/node/iter_gen.h"
#include "btree/path_gen.h"
#include "btree/types_gen.h"
#include "btree/node_gen.h"
#include "btree/bkey.h"
#include "btree/bkey/ops_gen.h"
#include "btree/trigger_gen.h"
#include "btree/cache_gen.h"

static inline u64 btree_ptr_hash_val(const struct bkey_i *k)
{
	switch (k->k.type) {
	case KEY_TYPE_btree_ptr:
		return *((u64 *) bkey_i_to_btree_ptr_c(k)->v.start);
	case KEY_TYPE_btree_ptr_v2:
		/*
		 * The cast/deref is only necessary to avoid sparse endianness
		 * warnings:
		 */
		return *((u64 *) &bkey_i_to_btree_ptr_v2_c(k)->v.seq);
	default:
		return 0;
	}
}

static inline struct btree *btree_node_mem_ptr(const struct bkey_i *k)
{
	return k->k.type == KEY_TYPE_btree_ptr_v2
		? (void *)(unsigned long)READ_ONCE(bkey_i_to_btree_ptr_v2_c(k)->v.mem_ptr)
		: NULL;
}

/*
 * btree_node_mem_ptr_set() updates mem_ptr in the parent's key with only a read
 * lock held, so it can change while we're copying the key out - and memcpy()
 * doesn't guarantee 8 byte atomic loads (x86 FSRM: rep movsb). Reload it after
 * copying a btree node pointer out of a node: a stale mem_ptr is fine (callers
 * check hash_val), a torn one is a wild pointer.
 */
static inline void btree_node_mem_ptr_reload(struct bkey_i *k, const struct bch_val *src)
{
	if (k->k.type == KEY_TYPE_btree_ptr_v2)
		bkey_i_to_btree_ptr_v2(k)->v.mem_ptr =
			READ_ONCE(((const struct bch_btree_ptr_v2 *) src)->mem_ptr);
}

/* is btree node in hash table? */
static inline bool btree_node_hashed(struct btree *b)
{
	return b->hash_val != 0;
}


static inline size_t btree_buf_bytes(const struct btree *b)
{
	return 1UL << b->byte_order;
}

static inline size_t btree_max_u64s(const struct bch_fs *c)
{
	return (c->opts.btree_node_size - sizeof(struct btree_node)) / sizeof(u64);
}

static inline size_t btree_sectors(const struct bch_fs *c)
{
	return c->opts.btree_node_size >> SECTOR_SHIFT;
}

static inline unsigned btree_blocks(const struct bch_fs *c)
{
	return btree_sectors(c) >> c->block_bits;
}

static inline bool bch2_btree_cache_should_throttle(struct bch_fs *c)
{
	return READ_ONCE(c->btree.cache.should_throttle);
}

#define BTREE_SPLIT_THRESHOLD(c)		(btree_max_u64s(c) * 3 / 4)

#define BTREE_FOREGROUND_MERGE_THRESHOLD(c)	(btree_max_u64s(c) * 1 / 3)
#define BTREE_FOREGROUND_MERGE_HIGHER_THRESHOLD(c)	(btree_max_u64s(c) * 3 / 5)
#define BTREE_FOREGROUND_MERGE_HYSTERESIS(c)			\
	(BTREE_FOREGROUND_MERGE_THRESHOLD(c) +			\
	 (BTREE_FOREGROUND_MERGE_THRESHOLD(c) >> 2))

static inline unsigned btree_id_nr_alive(struct bch_fs *c)
{
	return BTREE_ID_NR + READ_ONCE(c->btree.cache.roots_nr_extra);
}

/* A root's on-disk fields - key, level, alive: under root_lock */
static inline struct btree_root *bch2_btree_id_root(struct bch_fs *c, unsigned id)
{
	if (likely(id < BTREE_ID_NR)) {
		return &c->btree.cache.roots_known[id];
	} else {
		unsigned idx = id - BTREE_ID_NR;

		/* This can happen when we're called from btree_node_scan */
		if (idx >= c->btree.cache.roots_extra.nr)
			return NULL;

		return &c->btree.cache.roots_extra.data[idx];
	}
}

/*
 * Pack root pointer + level: low 3 bits encode b->c.level (BTREE_MAX_DEPTH
 * is 4, so values 0..3 fit comfortably; we reserve 3 bits to leave slack
 * for future depth bumps). struct btree alignment is well over 8 bytes so
 * the low bits are guaranteed free. Packing into a single word makes the
 * read in btree_path_lock_root naturally atomic — no torn read across b
 * and b->c.level — and avoids the cold-cacheline miss into the btree
 * node just to learn its level.
 */
#define BTREE_ROOT_LEVEL_MASK	((1UL << BTREE_ROOT_LEVEL_BITS) - 1)

static inline struct btree *bch2_btree_root_unpack_b(unsigned long v)
{
	return (struct btree *)(v & ~BTREE_ROOT_LEVEL_MASK);
}

static inline unsigned bch2_btree_root_unpack_level(unsigned long v)
{
	return v & BTREE_ROOT_LEVEL_MASK;
}

/*
 * Hot read of packed root pointer + level, which root_lock publishes: for
 * the known btrees, 4 cache lines.
 */
static inline unsigned long bch2_btree_id_root_packed(struct bch_fs *c, unsigned btree_id)
{
	return likely(btree_id < BTREE_ID_NR_MAX)
		? READ_ONCE(c->btree.cache.roots_b[btree_id])
		: 0;
}

static inline struct btree *bch2_btree_id_root_b(struct bch_fs *c, unsigned btree_id)
{
	return bch2_btree_root_unpack_b(bch2_btree_id_root_packed(c, btree_id));
}

/* The error reading the root left, published as the root is */
static inline int bch2_btree_id_root_error(struct bch_fs *c, unsigned btree_id)
{
	return likely(btree_id < BTREE_ID_NR_MAX)
		? READ_ONCE(c->btree.cache.roots_error[btree_id])
		: 0;
}

static inline int bch2_btree_id_root_set_error(struct bch_fs *c, unsigned btree_id, int err)
{
	guard(mutex)(&c->btree.cache.root_lock);
	WRITE_ONCE(c->btree.cache.roots_error[btree_id], err);
	return err;
}

static inline struct btree *btree_node_root(struct bch_fs *c, struct btree *b)
{
	return bch2_btree_id_root_b(c, b->c.btree_id);
}

static inline bool btree_node_is_root(struct bch_fs *c, struct btree *b)
{
	struct btree *root = btree_node_root(c, b);

	BUG_ON(b != root && b->c.level >= root->c.level);
	return b == root;
}

#endif /* _BCACHEFS_BTREE_CACHE_H */
