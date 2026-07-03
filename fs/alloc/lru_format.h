/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_LRU_FORMAT_H
#define _BCACHEFS_LRU_FORMAT_H

struct bch_lru {
	struct bch_val		v;
	__le64			idx;
} __packed __aligned(8);

#define BCH_LRU_TYPES()		\
	x(read)			\
	x(fragmentation)	\
	x(stripes)

enum bch_lru_type {
#define x(n) BCH_LRU_##n,
	BCH_LRU_TYPES()
#undef x
};

/*
 * LRU id space: read LRUs and bucket fragmentation LRUs are per-device,
 * id = range base + device index:
 */
#define BCH_LRU_READ_MAX			(1U << 13)
#define BCH_LRU_BUCKET_FRAGMENTATION_START	(1U << 13)
#define BCH_LRU_BUCKET_FRAGMENTATION_END	(2U << 13)

#define BCH_LRU_STRIPE_FRAGMENTATION		((1U << 16) - 2)

/*
 * Obsolete: the single fs-wide bucket fragmentation lru, replaced by the
 * per-device fragmentation lrus in per_dev_fragmentation_lru; stale entries
 * are deleted by check_lrus:
 */
#define BCH_LRU_BUCKET_FRAGMENTATION_OLD	((1U << 16) - 1)

#define LRU_TIME_BITS			48
#define LRU_TIME_MAX			((1ULL << LRU_TIME_BITS) - 1)

#endif /* _BCACHEFS_LRU_FORMAT_H */
