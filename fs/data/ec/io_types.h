/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_DATA_EC_IO_TYPES_H
#define _BCACHEFS_DATA_EC_IO_TYPES_H

#include "enum_kind.h"

struct ec_bio {
	struct bch_dev		*ca;
	struct ec_stripe_buf	*buf;
	size_t			idx;
	int			rw;
	u64			submit_time;
	struct bio		bio;
};

enum __enum_closed bch_stripe_buf_err {
	STRIPE_BUF_PRE_RECOV,
	STRIPE_BUF_POST_RECOV,
};

struct ec_stripe_buf {
	/* belongs to the buffer's owner, see bch2_ec_stripe_buf_move(): */
	struct closure		io;

	struct_group(contents,
	struct bch_fs		*c;

	/* might not be buffering the entire stripe: */
	unsigned		offset;
	unsigned		size;
	s16			err[2][BCH_BKEY_PTRS_MAX];
	void			*data[BCH_BKEY_PTRS_MAX];

	/* Stale when we read the stripe key, i.e. alloc inconsistency */
	unsigned long		stale[BITS_TO_LONGS(BCH_BKEY_PTRS_MAX)];

	struct bch_csum		csum_good[BCH_BKEY_PTRS_MAX];
	struct bch_csum		csum_bad[BCH_BKEY_PTRS_MAX];

	struct bkey_i_stripe	key;
	u64			pad[255];
	);
};

struct bch_read_bio;

#endif /* _BCACHEFS_DATA_EC_IO_TYPES_H */
