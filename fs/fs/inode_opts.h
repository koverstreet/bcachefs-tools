/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_INODE_OPTS_H
#define _BCACHEFS_INODE_OPTS_H

#include "fs/inode_format.h"

struct bch_inode_unpacked;

extern const char * const bch2_inode_opts[];

int bch2_opt_to_inode_opt(int);

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

#define inode_opt_get(_c, _inode, _name)			\
	((_inode)->bi_##_name ? (_inode)->bi_##_name - 1 : (_c)->opts._name)

struct bch_opts bch2_inode_opts_to_opts(struct bch_inode_unpacked *);
void bch2_inode_opts_get_inode(struct bch_fs *, struct bch_inode_unpacked *,
			       struct bch_inode_opts *);

bool bch2_reinherit_attrs(struct bch_inode_unpacked *, struct bch_inode_unpacked *);

#endif /* _BCACHEFS_INODE_OPTS_H */
