/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_LOGGED_OPS_H
#define _BCACHEFS_LOGGED_OPS_H

#include "btree/bkey.h"
#include "fs/logged_ops_gen.h"

/*
 * Fails once if logged_op_fail_next is armed for @type, standing in for a
 * crash at this point.
 *
 * cmpxchg: arming is by type, so two ops of that type in flight would both
 * fire.
 */
static inline int bch2_logged_op_inject_fail(struct bch_fs *c, unsigned type)
{
	if (unlikely(READ_ONCE(c->logged_ops.fail_next) == type) &&
	    cmpxchg(&c->logged_ops.fail_next, type, 0) == type)
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

#endif /* _BCACHEFS_LOGGED_OPS_H */
