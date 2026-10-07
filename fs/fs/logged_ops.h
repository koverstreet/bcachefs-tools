/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_LOGGED_OPS_H
#define _BCACHEFS_LOGGED_OPS_H

#include "btree/bkey.h"

/*
 * @early: background work that runs during recovery (copygc, reconcile) can
 * start these, so they're resumed by resume_logged_ops_early. The rest are only
 * started from userspace, and are resumed after fsck has repaired what they
 * walk. Either pass resumes only the ops from before this mount: see
 * bch2_logged_ops_note_unfinished().
 */
#define BCH_LOGGED_OPS()			\
	x(truncate,		false)		\
	x(finsert,		false)		\
	x(stripe_update,	true)		\
	x(inode_opt_propagate,	false)

/*
 * Fails once if logged_op_fail_next is armed for @type, standing in for a
 * crash at this point.
 *
 * cmpxchg: arming is by type, so two ops of that type in flight would both
 * fire.
 */
static inline int bch2_logged_op_inject_fail(struct bch_fs *c, unsigned type)
{
	if (unlikely(READ_ONCE(c->logged_op_fail_next) == type) &&
	    cmpxchg(&c->logged_op_fail_next, type, 0) == type)
		return bch_err_throw(c, injected_logged_op_fail);
	return 0;
}

/*
 * Every op with a cursor advances it through here, so it's the one place to
 * interrupt one: the op is then left at a cursor the real code produced.
 */
static inline int bch2_logged_op_update(struct btree_trans *trans, struct bkey_i *op)
{
	return bch2_logged_op_inject_fail(trans->c, op->k.type) ?:
		bch2_btree_insert_trans(trans, BTREE_ID_logged_ops, op, BTREE_ITER_cached);
}

/* Names as written to the sysfs knob, indexed as BCH_LOGGED_OPS() is: */
extern const char * const bch2_logged_ops[];

int bch2_logged_op_fail_next_parse(const char *, unsigned *);
void bch2_logged_op_fail_next_to_text(struct printbuf *, struct bch_fs *);

int bch2_logged_ops_note_unfinished(struct bch_fs *);
int bch2_resume_logged_ops_early(struct bch_fs *);
int bch2_resume_logged_ops(struct bch_fs *);
int __bch2_logged_op_start(struct btree_trans *, struct bkey_i *);
int bch2_logged_op_start(struct btree_trans *, struct bkey_i *);
int bch2_logged_op_finish(struct btree_trans *, struct bkey_i *, int, unsigned);

#endif /* _BCACHEFS_LOGGED_OPS_H */
