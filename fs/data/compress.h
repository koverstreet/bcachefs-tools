/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_DATA_COMPRESS_H
#define _BCACHEFS_DATA_COMPRESS_H

#include "data/extents_gen.h"

static const unsigned __bch2_compression_opt_to_type[] = {
#define x(t, n) [BCH_COMPRESSION_OPT_##t] = BCH_COMPRESSION_TYPE_##t,
	BCH_COMPRESSION_OPTS()
#undef x
};

#include "data/compress_defs_gen.h"

static inline enum bch_compression_type bch2_compression_opt_to_type(unsigned v)
{
	union bch_compression_opt opt = { .value = v };

	if (unlikely(opt.type >= ARRAY_SIZE(__bch2_compression_opt_to_type))) {
		/* As in bch2_csum_opt_to_type() - shouldn't happen, and
		 * shouldn't read off the end of the table if it does: */
		WARN_ONCE(1, "unknown compression opt %u", v);
		return BCH_COMPRESSION_TYPE_none;
	}

	return __bch2_compression_opt_to_type[opt.type];
}

#define bch2_opt_compression (struct bch_opt_fn) {		\
	.parse		= bch2_opt_compression_parse,		\
	.to_text	= bch2_opt_compression_to_text,		\
	.validate	= bch2_opt_compression_validate,	\
}

#endif /* _BCACHEFS_DATA_COMPRESS_H */
