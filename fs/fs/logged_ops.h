/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_LOGGED_OPS_H
#define _BCACHEFS_LOGGED_OPS_H

#include "btree/bkey.h"

#define BCH_LOGGED_OPS()			\
	x(truncate)				\
	x(finsert)				\
	x(stripe_update)				\
	x(inode_opt_propagate)

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

int bch2_resume_logged_ops(struct bch_fs *);
int __bch2_logged_op_start(struct btree_trans *, struct bkey_i *);
int bch2_logged_op_start(struct btree_trans *, struct bkey_i *);
int bch2_logged_op_finish(struct btree_trans *, struct bkey_i *, int);

#endif /* _BCACHEFS_LOGGED_OPS_H */
