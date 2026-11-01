/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_BKEY_CMP_H
#define _BCACHEFS_BKEY_CMP_H

#include "btree/bkey.h"

static inline int __bkey_cmp_bits(const u64 *l, const u64 *r,
				  unsigned nr_key_bits)
{
	u64 l_v, r_v;

	if (!nr_key_bits)
		return 0;

	/* for big endian, skip past header */
	nr_key_bits += high_bit_offset;
	l_v = *l & (~0ULL >> high_bit_offset);
	r_v = *r & (~0ULL >> high_bit_offset);

	while (1) {
		if (nr_key_bits < 64) {
			l_v >>= 64 - nr_key_bits;
			r_v >>= 64 - nr_key_bits;
			nr_key_bits = 0;
		} else {
			nr_key_bits -= 64;
		}

		if (!nr_key_bits || l_v != r_v)
			break;

		l = next_word(l);
		r = next_word(r);

		l_v = *l;
		r_v = *r;
	}

	return cmp_int(l_v, r_v);
}

static inline __pure __flatten
int __bch2_bkey_cmp_packed_format_checked_inlined(const struct bkey_packed *l,
					  const struct bkey_packed *r,
					  const struct btree *b)
{
	const struct bkey_format *f = &b->format;
	int ret;

	EBUG_ON(!bkey_packed(l) || !bkey_packed(r));
	EBUG_ON(b->nr_key_bits != bkey_format_key_bits(f));

	ret = __bkey_cmp_bits(high_word(f, l),
			      high_word(f, r),
			      b->nr_key_bits);

	EBUG_ON(ret != bpos_cmp(bkey_unpack_pos(b, l),
				bkey_unpack_pos(b, r)));
	return ret;
}

static inline __pure __flatten
int bch2_bkey_cmp_packed_inlined(const struct btree *b,
			 const struct bkey_packed *l,
			 const struct bkey_packed *r)
{
	if (likely(bkey_packed(l) && bkey_packed(r)))
		return __bch2_bkey_cmp_packed_format_checked_inlined(l, r, b);

	/*
	 * Declare in the cold mixed packed/unpacked branch so the
	 * CONFIG_INIT_STACK_ALL_PATTERN auto-init (5 x mov of the 0xFE
	 * pattern over sizeof(struct bkey) == 40 bytes) doesn't run on
	 * every hot-path call where the buffer is never touched.
	 */
	struct bkey unpacked;
	if (bkey_packed(l)) {
		__bkey_unpack_key_format_checked(b, &unpacked, l);
		l = (void *) &unpacked;
	} else if (bkey_packed(r)) {
		__bkey_unpack_key_format_checked(b, &unpacked, r);
		r = (void *) &unpacked;
	}

	return bpos_cmp(((struct bkey *) l)->p, ((struct bkey *) r)->p);
}

#endif /* _BCACHEFS_BKEY_CMP_H */
