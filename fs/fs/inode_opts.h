/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_INODE_OPTS_H
#define _BCACHEFS_INODE_OPTS_H

#include "btree/bkey_gen.h"
#include "fs/inode_format_gen.h"

#include "fs/inode_opts_gen.h"

extern const char * const bch2_inode_opts[];

/*
 * Options are stored with a +1 bias so that 0 means "not set": 1 is an
 * explicit "none", which is a different thing from inheriting the filesystem
 * default. Callers wanting the resolved value want inode_opt_get() or
 * bch2_inode_opts_get_inode() instead.
 */
static inline void bch2_inode_opt_set(struct bch_inode_unpacked *inode,
				      enum inode_opt_id id, u64 v)
{
	switch (id) {
#define x(_name, ...)							\
	case Inode_opt_##_name:						\
		inode->bi_##_name = v;					\
		break;
	BCH_INODE_OPTS()
#undef x
	default:
		BUG();
	}
}

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

/* Whether any per-inode option is set: what BCH_INODE_has_inode_opts records */
static inline bool bch2_inode_has_opts(const struct bch_inode_unpacked *inode)
{
#define x(_name, ...)	if (inode->bi_##_name) return true;
	BCH_INODE_OPTS()
#undef x
	return false;
}

#define inode_opt_get(_c, _inode, _name)			\
	((_inode)->bi_##_name ? (_inode)->bi_##_name - 1 : (_c)->opts._name)

int bch2_inode_opt_propagate_start(struct btree_trans *, u64, u32,
				   struct bkey_i_logged_op_inode_opt_propagate *);
int bch2_inode_opt_propagate_finish(struct btree_trans *,
				    struct bkey_i_logged_op_inode_opt_propagate *);

int bch2_resume_logged_op_inode_opt_propagate(struct btree_trans *, struct bkey_i *);
void bch2_logged_op_inode_opt_propagate_to_text(struct printbuf *, struct bch_fs *,
						struct bkey_s_c);

#define bch2_bkey_ops_logged_op_inode_opt_propagate ((struct bkey_ops) {	\
	.val_to_text	= bch2_logged_op_inode_opt_propagate_to_text,	\
})

#endif /* _BCACHEFS_INODE_OPTS_H */
