/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_ALLOC_BUCKETS_DEFS_H
#define _BCACHEFS_ALLOC_BUCKETS_DEFS_H

#include "enum_kind.h"

enum __enum_flags bch_reservation_flags {
	BCH_DISK_RESERVATION_NOFAIL	= 1 << 0,
	BCH_DISK_RESERVATION_PARTIAL	= 1 << 1,
};

struct disk_reservation_destructable {
	struct bch_fs			*c;
	struct disk_reservation		r;
};

#endif /* _BCACHEFS_ALLOC_BUCKETS_DEFS_H */
