/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_BTREE_ITER_TYPES_H
#define _BCACHEFS_BTREE_ITER_TYPES_H

struct trans_for_each_path_inorder_iter {
	btree_path_idx_t	sorted_idx;
	btree_path_idx_t	path_idx;
};

/*
 * Exempt bch2_trans_begin() from the dropped-updates warning for a scope (see
 * begin_may_drop_updates), restoring the previous value on exit, so exemptions
 * nest: an inner one ending doesn't end the outer.
 */
struct trans_may_drop_updates {
	struct btree_trans	*trans;
	bool			old;
};

#endif /* _BCACHEFS_BTREE_ITER_TYPES_H */
