/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_QUOTA_H
#define _BCACHEFS_QUOTA_H

#include "inode.h"
#include "fs/quota_gen.h"

#define bch2_bkey_ops_quota ((struct bkey_ops) {	\
	.key_validate	= bch2_quota_validate,		\
	.val_to_text	= bch2_quota_to_text,		\
})

static inline struct bch_qid bch_qid(struct bch_inode_unpacked *u)
{
	return (struct bch_qid) {
		.q[QTYP_USR] = u->bi_uid,
		.q[QTYP_GRP] = u->bi_gid,
		.q[QTYP_PRJ] = u->bi_project ? u->bi_project - 1 : 0,
	};
}

static inline unsigned enabled_qtypes(struct bch_fs *c)
{
	return ((c->opts.usrquota << QTYP_USR)|
		(c->opts.grpquota << QTYP_GRP)|
		(c->opts.prjquota << QTYP_PRJ));
}

#ifndef CONFIG_BCACHEFS_QUOTA

static inline int bch2_quota_acct(struct bch_fs *c, struct bch_qid qid,
				  enum quota_counters counter, s64 v,
				  enum quota_acct_mode mode)
{
	return 0;
}

static inline int bch2_quota_transfer(struct bch_fs *c, unsigned qtypes,
				      struct bch_qid dst,
				      struct bch_qid src, u64 space,
				      enum quota_acct_mode mode)
{
	return 0;
}

static inline int bch2_fs_quota_read(struct bch_fs *c) { return 0; }

#endif

#endif /* _BCACHEFS_QUOTA_H */
