// SPDX-License-Identifier: GPL-2.0

/*
 * erasure coding: opening an existing stripe - the open stripes table is
 * Rust's, in trigger.rs; this is here until Rust has btree node locks
 */

#include "bcachefs.h"

#include "btree/iter.h"
#include "btree/locking.h"

#include "data/ec/trigger.h"

/* Intent-locked existing key: 1 opened, 0 busy, negative error/restart. */
int bch2_stripe_tryget_existing(struct btree_iter *iter)
{
	struct btree_trans *trans = iter->trans;
	u64 idx = iter->pos.offset;

	EBUG_ON(iter->btree_id != BTREE_ID_stripes);
	BUG_ON(!btree_node_intent_locked(btree_iter_path(trans, iter), 0));

	if (bch2_stripe_is_open(trans->c, idx))
		return 0;

	/*
	 * The stripe trigger and device invalidation act on stripes that
	 * aren't open, and their decision can outlive a lock drop: commit
	 * relocks and retries without re-running triggers. Cycling a write
	 * lock advances the lock sequence, so that relock fails.
	 *
	 * The lock to cycle is the key cache entry: anything updating this key
	 * holds it, and an entry is only freed under its write lock - so if
	 * there's no entry, they're already invalidated. Our leaf intent lock
	 * keeps one from being created meanwhile (the fill write locks the
	 * leaf).
	 */
	CLASS(btree_iter, ck_iter)(trans, BTREE_ID_stripes, iter->pos,
				   BTREE_ITER_intent|
				   BTREE_ITER_cached|
				   BTREE_ITER_cached_nofill);
	try(bch2_btree_iter_traverse(&ck_iter));

	struct btree_path *ck_path = btree_iter_path(trans, &ck_iter);
	if (!ck_path->l[0].b)
		return bch2_stripe_tryget(trans->c, idx);

	/* Take the lock before opening, so a restart can't leak an open stripe: */
	try(bch2_btree_node_lock_write(trans, ck_path, &ck_path->l[0].b->c));
	int ret = bch2_stripe_tryget(trans->c, idx);
	bch2_btree_node_unlock_write(trans, ck_path, ck_path->l[0].b);
	return ret;
}
