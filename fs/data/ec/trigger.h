/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_DATA_EC_TRIGGER_H
#define _BCACHEFS_DATA_EC_TRIGGER_H
#include "data/ec/open_gen.h"
#include "data/ec/stripe_gen.h"
#include "data/ec/trigger_gen.h"

#define bch2_bkey_ops_stripe ((struct bkey_ops) {	\
	.key_validate	= bch2_stripe_validate,		\
	.val_to_text	= bch2_stripe_to_text,		\
	.swab		= bch2_ptr_swab,		\
	.trigger	= bch2_trigger_stripe,		\
	.check_repair	= bch2_check_fix_ptrs,		\
})

/*
 * A stripe's block counts, for btree/check.c's gc until it converts: the
 * layout is trigger.rs's.
 */

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

#endif /* _BCACHEFS_DATA_EC_TRIGGER_H */

