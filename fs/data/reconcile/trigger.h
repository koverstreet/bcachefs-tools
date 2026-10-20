/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_RECONCILE_TRIGGER_H
#define _BCACHEFS_RECONCILE_TRIGGER_H

#include "data/extents.h"
#include "data/reconcile/needs_reconcile_gen.h"
#include "data/reconcile/trigger_gen.h"
#include "fs/inode_opts_gen.h"

static inline struct bpos data_to_rb_work_pos(enum btree_id btree, struct bpos pos)
{
	if (btree == BTREE_ID_reflink ||
	    btree == BTREE_ID_stripes)
		pos = bpos_min(pos, POS(0, U64_MAX));

	if (btree == BTREE_ID_extents)
		pos = bpos_max(pos, POS(BCACHEFS_ROOT_INO, 0));

	if (btree == BTREE_ID_reflink)
		pos.inode++;
	return pos;
}

static inline struct bbpos rb_work_to_data_pos(struct bpos pos)
{
	if (!pos.inode)
		return BBPOS(BTREE_ID_stripes, pos);
	if (pos.inode < BCACHEFS_ROOT_INO) {
		--pos.inode;
		return BBPOS(BTREE_ID_reflink, pos);
	}
	return BBPOS(BTREE_ID_extents, pos);
}

static inline enum reconcile_work_id rb_work_id(const struct bch_extent_reconcile *r)
{
	if (!r || !r->need_rb)
		return RECONCILE_WORK_none;
	if (r->pending)
		return RECONCILE_WORK_pending;
	if (r->hipri)
		return RECONCILE_WORK_hipri;
	return RECONCILE_WORK_normal;
}

static inline enum reconcile_work_id rb_work_id_phys(enum reconcile_work_id w)
{
	return w == RECONCILE_WORK_pending ? RECONCILE_WORK_none : w;
}

static inline struct bch_extent_reconcile io_opts_to_reconcile_opts(struct bch_fs *c,
								    struct bch_inode_opts *opts)
{
	return (struct bch_extent_reconcile) {
		.type = BIT(BCH_EXTENT_ENTRY_reconcile),
#define x(_name)							\
		._name = opts->_name,					\
		._name##_from_inode = opts->_name##_from_inode,
		BCH_RECONCILE_OPTS()
#undef x
	};
};

static inline struct bch_backpointer rb_bp(enum btree_id btree, unsigned level, struct bkey_s_c k)
{
	return (struct bch_backpointer) {
		.btree_id	= btree,
		.level		= level,
		.pos		= k.k->p,
	};
}

static inline bool extent_has_rotational(struct bch_fs *c, struct bkey_s_c k)
{
	struct bkey_ptrs_c ptrs = bch2_bkey_ptrs_c(k);

	bkey_for_each_ptr(ptrs, ptr)
		if (bch2_dev_rotational(c, ptr->dev))
			return true;
	return false;
}

static inline struct per_snapshot_io_opts per_snapshot_io_opts_init(struct bch_fs *c)
{
	return (struct per_snapshot_io_opts) {
		/* io_opts->fs_io_opts will be initialized when we know the key type */
		.fs_io_opts.change_cookie = c->opt_change_cookie - 1,
	};
}

static inline void per_snapshot_io_opts_exit(struct per_snapshot_io_opts *io_opts)
{
	darray_exit(&io_opts->d);
}

DEFINE_CLASS(per_snapshot_io_opts, struct per_snapshot_io_opts,
	     per_snapshot_io_opts_exit(&_T),
	     per_snapshot_io_opts_init(c),
	     struct bch_fs *c);

#endif /* _BCACHEFS_RECONCILE_TRIGGER_H */
