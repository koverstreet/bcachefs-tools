/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_IO_MISC_H
#define _BCACHEFS_IO_MISC_H
#include "data/io_misc_gen.h"

int bch2_extent_fallocate(struct btree_trans *, subvol_inum, struct btree_iter *,
			  u64, struct bch_inode_opts, s64 *,
			  struct write_point_specifier);

#define bch2_bkey_ops_logged_op_truncate ((struct bkey_ops) {	\
	.val_to_text	= bch2_logged_op_truncate_to_text,	\
})

#define bch2_bkey_ops_logged_op_finsert ((struct bkey_ops) {	\
	.val_to_text	= bch2_logged_op_finsert_to_text,	\
})

#endif /* _BCACHEFS_IO_MISC_H */
