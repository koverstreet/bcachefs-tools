// SPDX-License-Identifier: GPL-2.0
#include "bcachefs.h"

#include "btree/cache.h"
#include "btree/write.h"

static bool __bch2_btree_flush_all(struct bch_fs *c, unsigned flag)
{
	struct bch_fs_btree_cache *bc = &c->btree.cache;
	bool ret = false;

	if (!bc->table_init_done)
		return false;

	/*
	 * Each pass below picks one still-flagged node, drops the iteration
	 * lock, then waits for the bit to clear before re-walking. We don't
	 * want to hold rcu_read_lock or bc->lock across an arbitrarily-long
	 * wait.
	 */

	/* Hashed pass: walk the rhashtable. */
	while (true) {
		struct bucket_table *tbl;
		struct rhash_head *pos;
		struct btree *waiting = NULL, *b;
		unsigned i;

		rcu_read_lock();
		for_each_cached_btree(b, c, tbl, i, pos)
			if (test_bit(flag, &b->flags)) {
				waiting = b;
				break;
			}
		rcu_read_unlock();

		if (!waiting)
			break;

		bch2_btree_node_wait_on_flag(waiting, flag);
		ret = true;
	}

	/*
	 * Freeable pass.
	 *
	 * The cache state machine permits FREEABLE nodes to have a write in
	 * flight: bch2_btree_node_write_done_clean only transitions DIRTY →
	 * CLEAN when the node is currently DIRTY, so when reclaim moves a
	 * node to FREEABLE while its write is still in flight the node
	 * legitimately stays on bc->freeable until the write finishes (see
	 * the comment on bch2_btree_node_write_done_clean).
	 *
	 * for_each_cached_btree iterates the rhashtable, which doesn't
	 * include hash-removed (FREEABLE / FREED) nodes, so the pass above
	 * misses these. Wait on bc->freeable too — going-RO needs drained
	 * writes for the BUG_ONs in bch2_fs_btree_cache_exit's freeable walk.
	 */
	while (true) {
		struct btree *waiting = NULL, *b;

		scoped_guard(mutex_noio, &bc->lock)
			list_for_each_entry(b, &bc->freeable, list)
				if (test_bit(flag, &b->flags)) {
					waiting = b;
					break;
				}

		if (!waiting)
			break;

		bch2_btree_node_wait_on_flag(waiting, flag);
		ret = true;
	}

	return ret;
}

bool bch2_btree_flush_all_reads(struct bch_fs *c)
{
	return __bch2_btree_flush_all(c, BTREE_NODE_read_in_flight);
}

bool bch2_btree_flush_all_writes(struct bch_fs *c)
{
	return __bch2_btree_flush_all(c, BTREE_NODE_write_in_flight);
}
