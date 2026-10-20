/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_LRU_H
#define _BCACHEFS_LRU_H

/* The lru btree is lru.rs's; these inlines are its position encoding, for C */
#include "alloc/lru_gen.h"

static inline u64 lru_pos_id(struct bpos pos)
{
	return pos.inode >> LRU_TIME_BITS;
}

static inline u64 lru_pos_time(struct bpos pos)
{
	return pos.inode & ~(~0ULL << LRU_TIME_BITS);
}

static inline struct bpos lru_pos(u16 lru_id, u64 dev_bucket, u64 time)
{
	struct bpos pos = POS(((u64) lru_id << LRU_TIME_BITS)|time, dev_bucket);

	EBUG_ON(time > LRU_TIME_MAX);
	EBUG_ON(lru_pos_id(pos) != lru_id);
	EBUG_ON(lru_pos_time(pos) != time);
	EBUG_ON(pos.offset != dev_bucket);

	return pos;
}

static inline struct bpos lru_start(u16 lru_id)
{
	return lru_pos(lru_id, 0, 0);
}

static inline struct bpos lru_end(u16 lru_id)
{
	return lru_pos(lru_id, U64_MAX, LRU_TIME_MAX);
}

static inline u16 bucket_fragmentation_lru(unsigned dev)
{
	return BCH_LRU_BUCKET_FRAGMENTATION_START + dev;
}

#define bch2_bkey_ops_lru ((struct bkey_ops) {	\
	.key_validate	= bch2_lru_validate,	\
	.val_to_text	= bch2_lru_to_text,	\
})

static inline int bch2_lru_change(struct btree_trans *trans,
		      u16 lru_id, u64 dev_bucket,
		      u64 old_time, u64 new_time)
{
	return old_time != new_time
		? __bch2_lru_change(trans, lru_id, dev_bucket, old_time, new_time)
		: 0;
}

#endif /* _BCACHEFS_LRU_H */
