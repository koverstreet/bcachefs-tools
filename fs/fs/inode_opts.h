/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_INODE_OPTS_H
#define _BCACHEFS_INODE_OPTS_H

#include "btree/bkey_gen.h"
#include "fs/inode_format_gen.h"

#include "fs/inode_opts_gen.h"

/*
 * Options are stored with a +1 bias so that 0 means "not set": 1 is an
 * explicit "none", which is a different thing from inheriting the filesystem
 * default. Callers wanting the resolved value want
 * bch2_inode_opts_get_inode() instead.
 */
static inline u64 bch2_inode_opt_get(struct bch_inode_unpacked *inode,
				     enum inode_opt_id id)
{
	switch (id) {
#define x(_name, ...)							\
	case Inode_opt_##_name:						\
		return inode->bi_##_name;
	BCH_INODE_OPTS()
#undef x
	default:
		BUG();
	}
}

#define bch2_bkey_ops_logged_op_inode_opt_propagate ((struct bkey_ops) {	\
	.val_to_text	= bch2_logged_op_inode_opt_propagate_to_text,	\
})

#endif /* _BCACHEFS_INODE_OPTS_H */
