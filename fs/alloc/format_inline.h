/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_ALLOC_FORMAT_INLINE_H
#define _BCACHEFS_ALLOC_FORMAT_INLINE_H

/* Included at the end of alloc/format.h. */

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

static inline bool bucket_data_type_mismatch(enum bch_data_type bucket,
					     enum bch_data_type ptr)
{
	return !data_type_is_empty(bucket) &&
		bucket != BCH_DATA_multiple &&
		bucket_data_type(bucket) != bucket_data_type(ptr);
}

#endif /* _BCACHEFS_ALLOC_FORMAT_INLINE_H */
