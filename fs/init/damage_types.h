/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_DAMAGE_TYPES_H
#define _BCACHEFS_DAMAGE_TYPES_H

#include "util/darray.h"

/*
 * The key range of an extents btree node we lost.
 *
 * Stashed at the site that loses the node, because that is the last moment
 * the range is known: afterwards an inode that had every extent in the node
 * is indistinguishable from a sparse one. check_extents turns the stashed
 * ranges into per-inode damage records, and bch2_btree_lost_data() - called
 * at the same sites - is what schedules it.
 */
typedef struct {
	struct bpos	start, end;
} lost_extents_range;

DEFINE_DARRAY(lost_extents_range);

#endif /* _BCACHEFS_DAMAGE_TYPES_H */
