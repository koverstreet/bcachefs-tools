/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_DATA_EC_TRIGGER_H
#define _BCACHEFS_DATA_EC_TRIGGER_H
#include "data/ec/trigger_gen.h"

int bch2_trigger_stripe(struct btree_trans *, struct btree_trigger_op);

#define bch2_bkey_ops_stripe ((struct bkey_ops) {	\
	.key_validate	= bch2_stripe_validate,		\
	.val_to_text	= bch2_stripe_to_text,		\
	.swab		= bch2_ptr_swab,		\
	.trigger	= bch2_trigger_stripe,		\
	.check_repair	= bch2_check_fix_ptrs,		\
})

static inline unsigned stripe_csums_per_device(const struct bch_stripe *s)
{
	return DIV_ROUND_UP(le16_to_cpu(s->sectors),
			    1 << s->csum_granularity_bits);
}

/*
 * Checksum types are not validated - a stripe written by a newer version may
 * use one we don't know - so this is the only place the table is indexed, and
 * an unknown type takes the widest checksum we have, as bch2_csum_to_text()
 * does. The offsets that result are still bounded by the caller against
 * bkey_val_bytes().
 */
static inline unsigned stripe_csum_bytes(const struct bch_stripe *s)
{
	return s->csum_type < BCH_CSUM_NR ? bch_crc_bytes[s->csum_type] : 16;
}

/*
 * The checksums precede the block sector counts in the value and their size
 * comes from csum_type, so with a type we don't know we can't locate the
 * blockcounts at all - see struct bch_stripe. Validation lets such a key
 * through on purpose, so that a versioning mistake costs nothing worse than an
 * error and the stripe stays on disk and readable; what we can't do is update
 * it, because every offset we'd compute is guesswork.
 *
 * @s may be NULL, for a trigger with no old or no new key.
 */
static inline int stripe_csum_type_check(struct bch_fs *c,
					 const struct bch_stripe *s)
{
	return s && s->csum_type >= BCH_CSUM_NR
		? bch_err_throw(c, stripe_unknown_csum_type)
		: 0;
}

static inline unsigned stripe_csum_offset(const struct bch_stripe *s,
					  unsigned dev, unsigned csum_idx)
{
	unsigned csum_bytes = stripe_csum_bytes(s);

	return sizeof(struct bch_stripe) +
		sizeof(struct bch_extent_ptr) * s->nr_blocks +
		(dev * stripe_csums_per_device(s) + csum_idx) * csum_bytes;
}

static inline unsigned stripe_blockcount_offset(const struct bch_stripe *s,
						unsigned idx)
{
	return stripe_csum_offset(s, s->nr_blocks, 0) +
		sizeof(u16) * idx;
}

static inline unsigned stripe_blockcount_get(const struct bch_stripe *s,
					     unsigned idx)
{
	return le16_to_cpup((void *) s + stripe_blockcount_offset(s, idx));
}

static inline void stripe_blockcount_set(struct bch_stripe *s,
					 unsigned idx, unsigned v)
{
	__le16 *p = (void *) s + stripe_blockcount_offset(s, idx);

	*p = cpu_to_le16(v);
}

static inline unsigned stripe_val_u64s(const struct bch_stripe *s)
{
	return DIV_ROUND_UP(stripe_blockcount_offset(s, s->nr_blocks),
			    sizeof(u64));
}

static inline void *stripe_csum(struct bch_stripe *s,
				unsigned block, unsigned csum_idx)
{
	EBUG_ON(block >= s->nr_blocks);
	EBUG_ON(csum_idx >= stripe_csums_per_device(s));

	return (void *) s + stripe_csum_offset(s, block, csum_idx);
}

static inline struct bch_csum stripe_csum_get(struct bch_stripe *s,
				   unsigned block, unsigned csum_idx)
{
	struct bch_csum csum = { 0 };

	memcpy(&csum, stripe_csum(s, block, csum_idx), stripe_csum_bytes(s));
	return csum;
}

static inline void stripe_csum_set(struct bch_stripe *s,
				   unsigned block, unsigned csum_idx,
				   struct bch_csum csum)
{
	memcpy(stripe_csum(s, block, csum_idx), &csum, stripe_csum_bytes(s));
}

/*
 * Pool widening target: how many data blocks the current device pool
 * could supply to a stripe with this much parity overhead. Capped at
 * BCH_BKEY_PTRS_MAX, and at @max_data_blocks if nonzero (the
 * c->opts.ec_max_data_blocks fs-level cap).
 */
static inline unsigned stripe_widen_target_nr_data(unsigned nr_devs,
						   unsigned nr_redundant,
						   unsigned max_data_blocks)
{
	if (nr_devs <= nr_redundant)
		return 0;

	unsigned t = min_t(unsigned, nr_devs, BCH_BKEY_PTRS_MAX) - nr_redundant;
	if (max_data_blocks)
		t = min(t, max_data_blocks);
	return t;
}

/* The clamped (target - actual) delta to store in bch_stripe.can_widen. */
static inline u8 stripe_widen_value(unsigned target_nr_data, unsigned actual_nr_data)
{
	return clamp_t(int, (int)target_nr_data - (int)actual_nr_data, 0, 7);
}

static inline bool __bch2_ptr_matches_stripe(const struct bch_extent_ptr *stripe_ptr,
					     const struct bch_extent_ptr *data_ptr,
					     unsigned sectors)
{
	return  (data_ptr->dev    == stripe_ptr->dev ||
		 data_ptr->dev    == BCH_SB_MEMBER_INVALID ||
		 stripe_ptr->dev  == BCH_SB_MEMBER_INVALID) &&
		data_ptr->generation    == stripe_ptr->generation &&
		data_ptr->offset >= stripe_ptr->offset &&
		data_ptr->offset  < stripe_ptr->offset + sectors;
}

static inline bool bch2_ptr_matches_stripe(const struct bch_stripe *s,
					   struct extent_ptr_decoded p)
{
	unsigned nr_data = s->nr_blocks - s->nr_redundant;

	BUG_ON(!p.has_ec);

	if (p.ec.block >= nr_data)
		return false;

	return __bch2_ptr_matches_stripe(&s->ptrs[p.ec.block], &p.ptr,
					 le16_to_cpu(s->sectors));
}

static inline bool bch2_ptr_matches_stripe_m(const struct gc_stripe *m,
					     struct extent_ptr_decoded p)
{
	unsigned nr_data = m->nr_blocks - m->nr_redundant;

	BUG_ON(!p.has_ec);

	if (p.ec.block >= nr_data)
		return false;

	return __bch2_ptr_matches_stripe(&m->ptrs[p.ec.block], &p.ptr,
					 m->sectors);
}

static inline void gc_stripe_unlock(struct gc_stripe *s)
{
	bch2_byte_unlock(&s->lock);
}

static inline void gc_stripe_lock(struct gc_stripe *s)
{
	bch2_byte_lock(&s->lock);
}

int bch2_ec_stripe_mem_alloc(struct btree_trans *, struct btree_iter *);

bool bch2_bucket_has_new_stripe(struct bch_fs *, u64);

void bch2_stripe_new_buckets_add(struct bch_fs *c, struct ec_stripe_new *s);
void bch2_stripe_new_buckets_del(struct bch_fs *, struct ec_stripe_new *);

bool bch2_stripe_is_open(struct bch_fs *, u64);

/* Hash-only claim for an unallocated stripe slot. */
bool bch2_stripe_handle_tryget(struct bch_fs *, struct ec_stripe_handle *, u64);
void bch2_stripe_handle_move(struct bch_fs *, struct ec_stripe_handle *,
			     struct ec_stripe_handle *);

#endif /* _BCACHEFS_DATA_EC_TRIGGER_H */

