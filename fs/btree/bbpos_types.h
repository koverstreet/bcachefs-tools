/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_BBPOS_TYPES_H
#define _BCACHEFS_BBPOS_TYPES_H

struct bbpos {
	enum btree_id		btree;
	struct bpos		pos;
};

#define BBPOS_MIN	BBPOS(0, POS_MIN)
#define BBPOS_MAX	BBPOS(BTREE_ID_NR - 1, SPOS_MAX)

/*
 * Layout is padding-free (bpos is __packed __aligned(4)), so a blbpos can be
 * used directly as a memcmp/rhashtable key.
 */
struct blbpos {
	enum btree_id		btree:16;
	u16			level;
	struct bpos		pos;
};

#include "btree/bbpos_types_inline.h"

#endif /* _BCACHEFS_BBPOS_TYPES_H */
