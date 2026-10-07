/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_BTREE_BBPOS_TYPES_INLINE_H
#define _BCACHEFS_BTREE_BBPOS_TYPES_INLINE_H

/* Included at the end of btree/bbpos_types.h. */

static inline struct bbpos BBPOS(enum btree_id btree, struct bpos pos)
{
	return (struct bbpos) { btree, pos };
}

static inline struct blbpos BLBPOS(enum btree_id btree, unsigned level, struct bpos pos)
{
	return (struct blbpos) { btree, level, pos };
}

#endif /* _BCACHEFS_BTREE_BBPOS_TYPES_INLINE_H */
