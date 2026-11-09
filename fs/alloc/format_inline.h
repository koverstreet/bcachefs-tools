/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_ALLOC_FORMAT_INLINE_H
#define _BCACHEFS_ALLOC_FORMAT_INLINE_H

/* Included after alloc/format_gen.h. */

/* Data type computation */

/*
 * Normalize data_type to the type of data stored in the bucket: cached and
 * stripe data are both user data from the bucket's perspective.
 */
static inline enum bch_data_type bucket_data_type(enum bch_data_type data_type)
{
	switch (data_type) {
	case BCH_DATA_cached:
	case BCH_DATA_stripe:
		return BCH_DATA_user;
	default:
		return data_type;
	}
}

#endif /* _BCACHEFS_ALLOC_FORMAT_INLINE_H */
