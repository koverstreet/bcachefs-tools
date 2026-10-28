/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_ALLOC_BACKGROUND_H
#define _BCACHEFS_ALLOC_BACKGROUND_H

#include "bcachefs.h"
#include "alloc/types_gen.h"
#include "alloc/buckets.h"
#include "alloc/background_gen.h"
#include "alloc/capacity_gen.h"

/* Bucket position helpers */

static inline bool bch2_dev_bucket_exists(struct bch_fs *c, struct bpos pos)
{
	guard(rcu)();
	struct bch_dev *ca = bch2_dev_rcu_noerror(c, pos.inode);
	return ca && bucket_valid(ca, pos.offset);
}

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

static inline bool data_type_movable(enum bch_data_type type)
{
	return (1U << type) & DATA_TYPES_MOVABLE;
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

static inline void alloc_data_type_set(struct bch_alloc_v4 *a, enum bch_data_type data_type)
{
	a->data_type = alloc_data_type(*a, data_type);
}

#define BCH_FREESPACE_GENBITS_NR	(((unsigned) U8_MAX >> 4) + 1)

static inline u64 alloc_freespace_genbits(struct bch_alloc_v4 a)
{
	return ((u64) alloc_gc_gen(a) >> 4) << 56;
}

/* Alloc key operations */

static inline const struct bch_alloc_v4 *bch2_alloc_to_v4(struct bkey_s_c k, struct bch_alloc_v4 *convert)
{
	if (likely(k.k->type == KEY_TYPE_alloc_v4)) {
		const struct bch_alloc_v4 *ret = bkey_s_c_to_alloc_v4(k).v;
		if (BCH_ALLOC_V4_BACKPOINTERS_START(ret) == BCH_ALLOC_V4_U64s)
			return ret;
	}

	__bch2_alloc_to_v4(k, convert);
	return convert;
}

#define bch2_bkey_ops_alloc ((struct bkey_ops) {	\
	.key_validate	= bch2_alloc_v1_validate,	\
	.val_to_text	= bch2_alloc_to_text,		\
	.trigger	= bch2_trigger_alloc,		\
})

#define bch2_bkey_ops_alloc_v2 ((struct bkey_ops) {	\
	.key_validate	= bch2_alloc_v2_validate,	\
	.val_to_text	= bch2_alloc_to_text,		\
	.trigger	= bch2_trigger_alloc,		\
})

#define bch2_bkey_ops_alloc_v3 ((struct bkey_ops) {	\
	.key_validate	= bch2_alloc_v3_validate,	\
	.val_to_text	= bch2_alloc_to_text,		\
	.trigger	= bch2_trigger_alloc,		\
})

#define bch2_bkey_ops_alloc_v4 ((struct bkey_ops) {	\
	.key_validate	= bch2_alloc_v4_validate,	\
	.val_to_text	= bch2_alloc_v4_to_text,	\
	.swab		= bch2_alloc_v4_swab,		\
	.trigger	= bch2_trigger_alloc,		\
})

#define bch2_bkey_ops_bucket_gens ((struct bkey_ops) {	\
	.key_validate	= bch2_bucket_gens_validate,	\
	.val_to_text	= bch2_bucket_gens_to_text,	\
})

#endif /* _BCACHEFS_ALLOC_BACKGROUND_H */
