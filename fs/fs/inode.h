/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_INODE_H
#define _BCACHEFS_INODE_H

#include "btree/bkey.h"
#include "btree/bkey_methods.h"
#include "snapshots/snapshot.h"

#include <linux/hash.h>
#include <linux/sched.h>


int bch2_inode_validate(struct bch_fs *, struct bkey_s_c,
			const struct bkey_validate_context *);
int bch2_inode_v2_validate(struct bch_fs *, struct bkey_s_c,
			   const struct bkey_validate_context *);
int bch2_inode_v3_validate(struct bch_fs *, struct bkey_s_c,
			   const struct bkey_validate_context *);
void bch2_inode_to_text(struct printbuf *, struct bch_fs *, struct bkey_s_c);

int bch2_trigger_inode(struct btree_trans *, struct btree_trigger_op);

#define bch2_bkey_ops_inode ((struct bkey_ops) {	\
	.key_validate	= bch2_inode_validate,		\
	.val_to_text	= bch2_inode_to_text,		\
	.trigger	= bch2_trigger_inode,		\
	.min_val_size	= 16,				\
})

#define bch2_bkey_ops_inode_v2 ((struct bkey_ops) {	\
	.key_validate	= bch2_inode_v2_validate,	\
	.val_to_text	= bch2_inode_to_text,		\
	.trigger	= bch2_trigger_inode,		\
	.min_val_size	= 32,				\
})

#define bch2_bkey_ops_inode_v3 ((struct bkey_ops) {	\
	.key_validate	= bch2_inode_v3_validate,	\
	.val_to_text	= bch2_inode_to_text,		\
	.trigger	= bch2_trigger_inode,		\
	.min_val_size	= 48,				\
})

static inline bool bkey_is_inode(const struct bkey *k)
{
	return  k->type == KEY_TYPE_inode ||
		k->type == KEY_TYPE_inode_v2 ||
		k->type == KEY_TYPE_inode_v3;
}

int bch2_inode_generation_validate(struct bch_fs *, struct bkey_s_c,
				   const struct bkey_validate_context *);
void bch2_inode_generation_to_text(struct printbuf *, struct bch_fs *, struct bkey_s_c);

#define bch2_bkey_ops_inode_generation ((struct bkey_ops) {	\
	.key_validate	= bch2_inode_generation_validate,	\
	.val_to_text	= bch2_inode_generation_to_text,	\
	.min_val_size	= 8,					\
})

int bch2_inode_alloc_cursor_validate(struct bch_fs *, struct bkey_s_c,
				     const struct bkey_validate_context *);
void bch2_inode_alloc_cursor_to_text(struct printbuf *, struct bch_fs *, struct bkey_s_c);

#define bch2_bkey_ops_inode_alloc_cursor ((struct bkey_ops) {	\
	.key_validate	= bch2_inode_alloc_cursor_validate,	\
	.val_to_text	= bch2_inode_alloc_cursor_to_text,	\
	.min_val_size	= 16,					\
})
#include "fs/inode_gen.h"



#include "fs/inode_opts.h"

#define bch2_inode_peek(_trans, _iter, _inode, _inum, _flags)			\
	__bch2_inode_peek(_trans, _iter, _inode, _inum, _flags, __func__)

#define bch2_inode_find_by_inum_trans(_trans, _inum, _inode)			\
	__bch2_inode_find_by_inum_trans(_trans, _inum, _inode, __func__)

static inline int bch2_inode_find_by_inum_nowarn_trans(struct btree_trans *trans,
				  subvol_inum inum,
				  struct bch_inode_unpacked *inode)
{
	return __bch2_inode_find_by_inum_trans(trans, inum, inode, NULL);
}

static inline int bch2_inode_write(struct btree_trans *trans,
		     struct btree_iter *iter,
		     struct bch_inode_unpacked *inode)
{
	return bch2_inode_write_flags(trans, iter, inode, 0);
}

static inline u8 mode_to_type(umode_t mode)
{
	return (mode >> 12) & 15;
}

static inline u8 inode_d_type(struct bch_inode_unpacked *inode)
{
	return inode->bi_subvol ? DT_SUBVOL : mode_to_type(inode->bi_mode);
}

static inline bool bch2_inode_casefold(struct bch_fs *c, const struct bch_inode_unpacked *bi)
{
	/* inode opts are stored with a +1 bias: 0 means "unset, use fs opt" */
	return bi->bi_casefold
		? bi->bi_casefold - 1
		: c->opts.casefold;
}

/*
 * Subvolume root inodes are deleted by the subvolume deletion path -
 * subvolume state set to unlinked, VFS eviction, then the snapshot sweep
 * deletes the keys - never by the inode reaper (VFS eviction ->
 * bch2_inode_rm(), or the deleted_inodes btree): BCH_INODE_unlinked on a
 * subvolume root only records that no dirent points at it, not that the
 * inode reaper may delete it.
 */
static inline bool bch2_inode_is_subvolume_root(const struct bch_inode_unpacked *bi)
{
	return bi->bi_subvol != 0;
}

/* i_nlink: */

static inline unsigned nlink_bias(umode_t mode)
{
	return S_ISDIR(mode) ? 2 : 1;
}

static inline unsigned bch2_inode_nlink_get(struct bch_inode_unpacked *bi)
{
	return bi->bi_flags & BCH_INODE_unlinked
		  ? 0
		  : bi->bi_nlink + nlink_bias(bi->bi_mode);
}

#include "data/reconcile/trigger.h"

static inline struct bch_extent_reconcile
bch2_inode_reconcile_opts_get(struct bch_fs *c, struct bch_inode_unpacked *inode)
{
	struct bch_inode_opts io_opts;
	bch2_inode_opts_get_inode(c, inode, &io_opts);
	return io_opts_to_reconcile_opts(c, &io_opts);
}

#define BCACHEFS_ROOT_SUBVOL_INUM					\
	((subvol_inum) { BCACHEFS_ROOT_SUBVOL,	BCACHEFS_ROOT_INO })

static inline bool subvol_inum_eq(subvol_inum a, subvol_inum b)
{
	return a.subvol == b.subvol && a.inum == b.inum;
}

#endif /* _BCACHEFS_INODE_H */
