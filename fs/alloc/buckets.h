/* SPDX-License-Identifier: GPL-2.0 */
/*
 * Code for manipulating bucket marks for garbage collection.
 *
 * Copyright 2014 Datera, Inc.
 */

#ifndef _BUCKETS_H
#define _BUCKETS_H

#include "alloc/buckets_gen.h"
#include "alloc/capacity_gen.h"
#include "alloc/format_gen.h"
#include "data/extents.h"
#include "sb/members.h"

/* Bucket addressing: */

static inline u64 sector_to_bucket(const struct bch_dev *ca, sector_t s)
{
	return div_u64(s, ca->mi.bucket_size);
}

static inline sector_t bucket_to_sector(const struct bch_dev *ca, size_t b)
{
	return ((sector_t) b) * ca->mi.bucket_size;
}

static inline u64 sector_to_bucket_and_offset(const struct bch_dev *ca, sector_t s, u32 *offset)
{
	return div_u64_rem(s, ca->mi.bucket_size, offset);
}

/* Bucket locking: */

static inline void bucket_unlock(struct bucket *b)
{
	bch2_byte_unlock(&b->lock);
}

static inline void bucket_lock(struct bucket *b)
{
	bch2_byte_lock(&b->lock);
}

DEFINE_GUARD(bucket_lock, struct bucket *,
	     bucket_lock(_T),
	     bucket_unlock(_T));

/* Bucket generation: */

static inline struct bucket_gens *bucket_gens(struct bch_dev *ca)
{
	return rcu_dereference_check(ca->bucket_gens,
				     lockdep_is_held(&ca->fs->state_lock));
}

static inline u8 *bucket_gen(struct bch_dev *ca, size_t b)
{
	struct bucket_gens *gens = bucket_gens(ca);

	if (b - gens->first_bucket >= gens->nbuckets_minus_first)
		return NULL;
	return gens->b + b;
}

static inline int bucket_gen_get_rcu(struct bch_dev *ca, size_t b)
{
	u8 *gen = bucket_gen(ca, b);
	return gen ? *gen : -1;
}

static inline int bucket_gen_get(struct bch_dev *ca, size_t b)
{
	guard(rcu)();
	return bucket_gen_get_rcu(ca, b);
}

static inline int gen_cmp(u8 a, u8 b)
{
	return (s8) (a - b);
}

static inline int gen_after(u8 a, u8 b)
{
	return max(0, gen_cmp(a, b));
}

/* Pointer → bucket mapping: */

static inline size_t PTR_BUCKET_NR(const struct bch_dev *ca,
				   const struct bch_extent_ptr *ptr)
{
	return sector_to_bucket(ca, ptr->offset);
}

static inline struct bpos PTR_BUCKET_POS(const struct bch_dev *ca,
					 const struct bch_extent_ptr *ptr)
{
	return POS(ptr->dev, PTR_BUCKET_NR(ca, ptr));
}

static inline struct bpos PTR_BUCKET_POS_OFFSET(const struct bch_dev *ca,
						const struct bch_extent_ptr *ptr,
						u32 *bucket_offset)
{
	return POS(ptr->dev, sector_to_bucket_and_offset(ca, ptr->offset, bucket_offset));
}

/* Extent pointer helpers: */

static inline enum bch_data_type ptr_data_type(const struct bkey *k,
					       const struct bch_extent_ptr *ptr)
{
	if (bkey_is_btree_ptr(k))
		return BCH_DATA_btree;

	return ptr->cached ? BCH_DATA_cached : BCH_DATA_user;
}

static inline s64 ptr_disk_sectors(s64 sectors, struct extent_ptr_decoded p)
{
	EBUG_ON(sectors < 0);

	return crc_is_compressed(p.crc)
		? DIV_ROUND_UP_ULL(sectors * p.crc.compressed_size,
				   p.crc.uncompressed_size)
		: sectors;
}

/* Pointer staleness: */

static inline int dev_ptr_stale_rcu(struct bch_dev *ca, const struct bch_extent_ptr *ptr)
{
	int gen = bucket_gen_get_rcu(ca, PTR_BUCKET_NR(ca, ptr));
	return gen < 0 ? gen : gen_after(gen, ptr->generation);
}

/**
 * dev_ptr_stale() - check if a pointer points into a bucket that has been
 * invalidated.
 */
static inline int dev_ptr_stale(struct bch_dev *ca, const struct bch_extent_ptr *ptr)
{
	guard(rcu)();
	return dev_ptr_stale_rcu(ca, ptr);
}

/* Disk reservations: */

/*
 * Counts above BCH_REPLICAS_MAX are real - durability is per device - and share
 * the last slot, which is the conservative direction. 0 is a bug; the clamp
 * only keeps it in bounds.
 */
static inline unsigned disk_res_slot(unsigned nr_replicas)
{
	EBUG_ON(!nr_replicas);

	return clamp_t(unsigned, nr_replicas, 1, BCH_REPLICAS_MAX) - 1;
}

static inline void bch2_disk_reservation_put(struct bch_fs *c,
					     struct disk_reservation *res)
{
	if (res->sectors) {
		/* a lost count clamps to slot 0: these were charged elsewhere */
		WARN_ON(!res->nr_replicas);

		this_cpu_sub(c->capacity.pcpu->online_reserved[disk_res_slot(res->nr_replicas)],
			     res->sectors);
		res->sectors = 0;
	}
}

/*
 * A reservation lives entirely in one slot, so changing its count moves what it
 * holds. The counters are summed across cpus, so the two ops mustn't be split.
 */
static inline void disk_res_move_slot(struct bch_fs *c,
				      struct disk_reservation *res,
				      unsigned nr_replicas)
{
	if (res->sectors) {
		unsigned old = disk_res_slot(res->nr_replicas);
		unsigned new = disk_res_slot(nr_replicas);

		scoped_guard(preempt)
			if (old != new) {
				this_cpu_sub(c->capacity.pcpu->online_reserved[old],
					     res->sectors);
				this_cpu_add(c->capacity.pcpu->online_reserved[new],
					     res->sectors);
			}
	}

	res->nr_replicas = nr_replicas;
}

/*
 * Charging only ever raises the count: erring high reserves more than we need,
 * erring low reserves space we can't place. Only write_degraded lowers it.
 */
static inline void bch2_disk_reservation_set_nr_replicas(struct bch_fs *c,
							 struct disk_reservation *res,
							 unsigned nr_replicas)
{
	if (nr_replicas > res->nr_replicas)
		disk_res_move_slot(c, res, nr_replicas);
}

/*
 * In physical sectors, for the three callers whose number isn't sectors *
 * nr_replicas: overwrites, which credit back the old key's copies. A smell -
 * that arithmetic probably belongs elsewhere, and when it goes, so does this.
 */
static inline int __bch2_disk_reservation_add(struct bch_fs *c,
					      struct disk_reservation *res,
					      u64 sectors, unsigned nr_replicas,
					      int flags)
{
	bch2_disk_reservation_set_nr_replicas(c, res, nr_replicas);

	/*
	 * Not a hard BUG_ON: bi_data_replicas isn't validated, so a damaged
	 * inode can reach here with a zero count.
	 */
	EBUG_ON(sectors && !res->nr_replicas);

	/* nothing to reserve: e.g. fallocate over a range that has enough copies */
	if (!sectors)
		return 0;

#ifdef __KERNEL__
	unsigned slot = disk_res_slot(res->nr_replicas);
	u64 old, new;

	old = this_cpu_read(c->capacity.pcpu->sectors_available[slot]);
	do {
		if (sectors > old)
			return bch2_disk_reservation_add_slowpath(c, res, sectors, flags);

		new = old - sectors;
	} while (!this_cpu_try_cmpxchg(c->capacity.pcpu->sectors_available[slot], &old, new));

	this_cpu_add(c->capacity.pcpu->online_reserved[slot], sectors);
	res->sectors			+= sectors;
	return 0;
#else
	return bch2_disk_reservation_add_slowpath(c, res, sectors, flags);
#endif
}

DEFINE_CLASS(disk_reservation, struct disk_reservation_destructable,
	     bch2_disk_reservation_put(_T.r.c, &_T.r),
	     (struct disk_reservation_destructable) { .r.c = c },
	     struct bch_fs *c);

static inline u64 avail_factor(u64 r)
{
	return div_u64(r << RESERVE_FACTOR, (1 << RESERVE_FACTOR) + 1);
}

#endif /* _BUCKETS_H */
