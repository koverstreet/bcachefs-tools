/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_ALLOC_BACKGROUND_H
#define _BCACHEFS_ALLOC_BACKGROUND_H

#include "bcachefs.h"
#include "alloc/types_gen.h"
#include "alloc/buckets.h"
#include "alloc/background_gen.h"
#include "alloc/capacity_gen.h"

/* Bucket position helpers */

static inline u64 bucket_to_u64(struct bpos bucket)
{
	return (bucket.inode << 48) | (bucket.offset & ~(~0ULL << 48));
}

static inline struct bpos u64_to_bucket(u64 bucket)
{
	return POS(bucket >> 48, bucket & ~(~0ULL << 48));
}

/* Generation tracking */

static inline u8 alloc_gc_gen(struct bch_alloc_v4 a)
{
	return a.generation - a.oldest_gen;
}

static inline s64 bch2_bucket_sectors_dirty(struct bch_alloc_v4 a)
{
	return a.stripe_sectors + a.dirty_sectors;
}

/*
 * Compute data_type from bucket state. The data_type parameter is a hint for
 * what kind of data the bucket contains; the actual type is determined by
 * sector counts, stripe_refcount, and gc_gen.
 *
 * For non-empty buckets the result is fully determined here. For empty
 * buckets, need_discard is sticky (set by bch2_trigger_alloc() on
 * non-empty -> empty, cleared by the discard path), and free vs need_gc_gens
 * is derived from gc_gen.
 */
static inline enum bch_data_type alloc_data_type(struct bch_alloc_v4 a,
						 enum bch_data_type data_type)
{
	if (a.data_type == BCH_DATA_multiple)
		return BCH_DATA_multiple;
	if (a.stripe_refcount)
		return data_type == BCH_DATA_parity ? data_type : BCH_DATA_stripe;
	if (bch2_bucket_sectors_dirty(a))
		return bucket_data_type(data_type);
	if (a.cached_sectors)
		return BCH_DATA_cached;
	if (data_type == BCH_DATA_need_discard)
		return BCH_DATA_need_discard;
	return alloc_gc_gen(a) >= BUCKET_GC_GEN_MAX
		? BCH_DATA_need_gc_gens
		: BCH_DATA_free;
}

#define BCH_FREESPACE_GENBITS_NR	(((unsigned) U8_MAX >> 4) + 1)

/* Alloc key operations */

#endif /* _BCACHEFS_ALLOC_BACKGROUND_H */
