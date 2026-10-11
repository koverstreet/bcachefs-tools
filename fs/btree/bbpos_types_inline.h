/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_BTREE_BBPOS_TYPES_INLINE_H
#define _BCACHEFS_BTREE_BBPOS_TYPES_INLINE_H

/* Included after btree/bbpos_gen.h. */

static inline struct bbpos BBPOS(enum btree_id btree, struct bpos pos)
{
	return (struct bbpos) { btree, pos };
}

#define BBPOS_MIN	BBPOS(0, POS_MIN)
#define BBPOS_MAX	BBPOS(BTREE_ID_NR - 1, SPOS_MAX)

static inline struct blbpos BLBPOS(enum btree_id btree, unsigned level, struct bpos pos)
{
	return (struct blbpos) { btree, level, pos };
}

#endif /* _BCACHEFS_BTREE_BBPOS_TYPES_INLINE_H */
