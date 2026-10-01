/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_LOGGED_OPS_H
#define _BCACHEFS_LOGGED_OPS_H

#include "btree/bkey.h"

/*
 * @early: background work that runs during recovery (copygc, reconcile) can
 * start these, so they're resumed by resume_logged_ops_early, before that work
 * may run - any found after that could be live. The rest are only started from
 * userspace, and are resumed after fsck has repaired what they walk.
 */
#define BCH_LOGGED_OPS()			\
	x(truncate,		false)		\
	x(finsert,		false)		\
	x(stripe_update,	true)		\
	x(inode_opt_propagate,	false)

/*
 * Every op advances its cursor through here, so it's the one place to
 * interrupt one: the op is then left at a cursor the real code produced.
 *
 * cmpxchg: arming is by type, so two ops of that type in flight would both
 * fire.
 */
static inline int bch2_logged_op_update(struct btree_trans *trans, struct bkey_i *op)
{
	struct bch_fs *c = trans->c;

	if (unlikely(READ_ONCE(c->logged_op_fail_next) == op->k.type) &&
	    cmpxchg(&c->logged_op_fail_next, op->k.type, 0) == op->k.type)
		return bch_err_throw(c, injected_logged_op_fail);

	return bch2_btree_insert_trans(trans, BTREE_ID_logged_ops, op, BTREE_ITER_cached);
}

/* Names as written to the sysfs knob, indexed as BCH_LOGGED_OPS() is: */
extern const char * const bch2_logged_ops[];

int bch2_logged_op_fail_next_parse(const char *, unsigned *);
void bch2_logged_op_fail_next_to_text(struct printbuf *, struct bch_fs *);

int bch2_resume_logged_ops_early(struct bch_fs *);
int bch2_resume_logged_ops(struct bch_fs *);
int __bch2_logged_op_start(struct btree_trans *, struct bkey_i *);
int bch2_logged_op_start(struct btree_trans *, struct bkey_i *);
int bch2_logged_op_finish(struct btree_trans *, struct bkey_i *, int, unsigned);

#endif /* _BCACHEFS_LOGGED_OPS_H */
