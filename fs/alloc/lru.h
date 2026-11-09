/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_LRU_H
#define _BCACHEFS_LRU_H

/* The lru btree is lru.rs's; these inlines are its position encoding, for C */

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

#endif /* _BCACHEFS_LRU_H */
