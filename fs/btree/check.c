// SPDX-License-Identifier: GPL-2.0
/*
 * Copyright (C) 2010 Kent Overstreet <kent.overstreet@gmail.com>
 * Copyright (C) 2014 Datera Inc.
 */

#include "bcachefs.h"

#include "alloc/accounting.h"
#include "alloc/background.h"
#include "alloc/buckets.h"
#include "btree/cache.h"
#include "btree/iter.h"
#include "btree/update.h"
#include "data/reconcile/trigger.h"
#include "init/error.h"
#include "alloc/backpointers_gen.h"
#include "alloc/foreground.h"
#include "alloc/replicas.h"

#include "btree/bkey.h"
#include "btree/bkey/ops_gen.h"
#include "btree/trigger_gen.h"
#include "btree/bkey_buf.h"
#include "btree/check.h"
#include "btree/locking.h"
#include "btree/node_scan_gen.h"
#include "btree/interior.h"
#include "btree/journal_overlay.h"
#include "btree/read.h"

#include "data/extents.h"
#include "bcachefs_ioctl.h"
#include "btree/bbpos.h"
#include "data/read.h"
#include "data/write_gen.h"
#include "data/update_gen.h"
#include "data/move_gen.h"
#include "data/moving_update_gen.h"
#include "data/reflink_gen.h"

#include "btree/bbpos_gen.h"
#include "init/progress_gen.h"
#include "init/passes.h"
#include "init/recovery_gen.h"

#include "journal/journal.h"

#include "sb/counters.h"
#include "sb/io.h"

#include "util/enumerated_ref.h"

#include <linux/slab.h>
#include <linux/bitops.h>
#include <linux/freezer.h>
#include <linux/kthread.h>
#include <linux/preempt.h>
#include <linux/rcupdate.h>
#include <linux/sched/task.h>

static void btree_ptr_to_v2(struct btree *b, struct bkey_i_btree_ptr_v2 *dst)
{
	switch (b->key.k.type) {
	case KEY_TYPE_btree_ptr: {
		struct bkey_i_btree_ptr *src = bkey_i_to_btree_ptr(&b->key);

		dst->k.p		= src->k.p;
		dst->v.mem_ptr		= 0;
		dst->v.seq		= b->data->keys.seq;
		dst->v.sectors_written	= 0;
		dst->v.flags		= 0;
		dst->v.min_key		= b->data->min_key;
		set_bkey_val_bytes(&dst->k, sizeof(dst->v) + bkey_val_bytes(&src->k));
		memcpy(dst->v.start, src->v.start, bkey_val_bytes(&src->k));
		break;
	}
	case KEY_TYPE_btree_ptr_v2:
		bkey_copy(&dst->k_i, &b->key);
		break;
	default:
		BUG();
	}
}

static int set_node_min(struct bch_fs *c, struct btree *b, struct bpos new_min)
{
	struct bkey_i_btree_ptr_v2 *new;
	int ret;

	if (c->opts.verbose) {
		CLASS(printbuf, buf)();

		bch2_bkey_val_to_text(&buf, c, bkey_i_to_s_c(&b->key));
		prt_str(&buf, " -> ");
		bch2_bpos_to_text(&buf, new_min);

		bch_info(c, "%s(): %s", __func__, buf.buf);
	}

	new = kmalloc_array(BKEY_BTREE_PTR_U64s_MAX, sizeof(u64), GFP_KERNEL);
	if (!new)
		return bch_err_throw(c, ENOMEM_gc_repair_key);

	btree_ptr_to_v2(b, new);
	b->data->min_key	= new_min;
	new->v.min_key		= new_min;
	SET_BTREE_PTR_RANGE_UPDATED(&new->v, true);

	ret = bch2_journal_key_insert_take(c, b->c.btree_id, b->c.level + 1, &new->k_i);
	if (ret)
		return ret;

	bch2_btree_node_drop_keys_outside_node(b);
	bkey_copy(&b->key, &new->k_i);
	return 0;
}

static int set_node_max(struct btree_trans *trans, struct btree *b, struct bpos new_max)
{
	struct bch_fs *c = trans->c;
	struct bkey_i_btree_ptr_v2 *new;

	if (c->opts.verbose) {
		CLASS(printbuf, buf)();

		bch2_bkey_val_to_text(&buf, c, bkey_i_to_s_c(&b->key));
		prt_str(&buf, " -> ");
		bch2_bpos_to_text(&buf, new_max);

		bch_info(c, "%s(): %s", __func__, buf.buf);
	}

	try(bch2_journal_key_delete(c, b->c.btree_id, b->c.level + 1, b->key.k.p));

	new = kmalloc_array(BKEY_BTREE_PTR_U64s_MAX, sizeof(u64), GFP_KERNEL);
	if (!new)
		return bch_err_throw(c, ENOMEM_gc_repair_key);

	btree_ptr_to_v2(b, new);
	b->data->max_key	= new_max;
	new->k.p		= new_max;
	SET_BTREE_PTR_RANGE_UPDATED(&new->v, true);

	int ret = bch2_journal_key_insert_take(c, b->c.btree_id, b->c.level + 1, &new->k_i);
	if (ret)
		return ret;

	bch2_btree_node_drop_keys_outside_node(b);

	/*
	 * max_key is b->key.k.p, which is the cache's hash key - so unlike
	 * set_node_min() this has to leave the hash table and come back.
	 * Unhashing asserts SIX_LOCK_write, callers hold read, and six won't
	 * upgrade; roots additionally can't be unhashed while flagged.
	 */
	bool permanent = btree_node_permanent(b);
	if (permanent)
		bch2_btree_node_set_permanent(c, b, false);

	six_unlock_read(&b->c.lock);

	trans->locking_hash_val = 0;
	trans->locking_root_id	= -1;
	btree_node_lock_nopath(trans, &b->c, SIX_LOCK_intent, true, _THIS_IP_, false);
	btree_node_lock_nopath(trans, &b->c, SIX_LOCK_write, true, _THIS_IP_, false);

	/* unhash, rehash */
	BUG_ON(bch2_btree_node_transition_state(c, b, BTREE_NODE_CACHE_FREEABLE));
	bkey_copy(&b->key, &new->k_i);
	BUG_ON(bch2_btree_node_transition_state(c, b, btree_node_live_state(b)));

	six_unlock_write(&b->c.lock);
	six_unlock_intent(&b->c.lock);
	btree_node_lock_nopath(trans, &b->c, SIX_LOCK_read, true, _THIS_IP_, false);

	/*
	 * Still the root - but it was on bc->freeable with roots_b[] pointing at
	 * it, which only single-threaded recovery makes safe.
	 */
	if (permanent)
		bch2_btree_node_set_permanent(c, b, true);
	return 0;
}

/*
 * Commit the fsck "repaired" log entry queued by mustfix_fsck_err().
 *
 * Topology repair runs before we go rw and applies its fixes through journal
 * keys (not transactional updates), so nothing else here commits - but the log
 * is a transaction update, and the next bch2_trans_begin() would silently drop
 * it. Commit it here, at the check, *before* the mutation: a commit can return
 * a transaction restart (fault injection, journal, ...), and doing it first
 * means the restart re-runs only the check - which re-queues the log - never
 * the non-transactional mutation, which must run exactly once.
 */
static int commit_topology_repair_log(struct btree_trans *trans)
{
	return bch2_trans_has_updates(trans)
		? bch2_trans_commit(trans, NULL, NULL, BCH_TRANS_COMMIT_no_enospc)
		: 0;
}

static int btree_check_node_boundaries(struct btree_trans *trans, struct btree *b,
				       struct btree *prev, struct btree *cur)
{
	struct bch_fs *c = trans->c;
	struct bpos expected_start = !prev
		? b->data->min_key
		: bpos_successor(prev->key.k.p);
	CLASS(printbuf, buf)();
	int ret = 0;

	BUG_ON(b->key.k.type == KEY_TYPE_btree_ptr_v2 &&
	       !bpos_eq(bkey_i_to_btree_ptr_v2(&b->key)->v.min_key,
			b->data->min_key));

	if (bpos_eq(expected_start, cur->data->min_key))
		return 0;

	prt_printf(&buf, " at ");
	bch2_btree_id_level_to_text(&buf, b->c.btree_id, b->c.level);
	prt_printf(&buf, ":\nparent: ");
	bch2_bkey_val_to_text(&buf, c, bkey_i_to_s_c(&b->key));

	if (prev) {
		prt_printf(&buf, "\nprev: ");
		bch2_bkey_val_to_text(&buf, c, bkey_i_to_s_c(&prev->key));
	}

	prt_str(&buf, "\nnext: ");
	bch2_bkey_val_to_text(&buf, c, bkey_i_to_s_c(&cur->key));
	prt_newline(&buf);

	if (bpos_lt(expected_start, cur->data->min_key)) {				/* gap */
		size_t nodes_found = 0;

		if (b->c.level == 1 &&
		    btree_id_recovers_from_scan(b->c.btree_id)) {
			try(bch2_get_scanned_nodes(c, b->c.btree_id, 0,
						   expected_start,
						   bpos_predecessor(cur->data->min_key),
						   &buf, &nodes_found));
			if (!nodes_found)
				prt_printf(&buf, "btree node scan found no nodes this range\n");
		}

		if (mustfix_fsck_err(trans, btree_node_topology_gap_between_nodes,
				     "gap between btree nodes%s", buf.buf)) {
			try(commit_topology_repair_log(trans));
			if (nodes_found)
				return bch_err_throw(c, topology_repair_did_fill_from_scan);
			else
				return set_node_min(c, cur, expected_start);
		}
	} else {									/* overlap */
		if (prev && BTREE_NODE_SEQ(cur->data) > BTREE_NODE_SEQ(prev->data)) {	/* cur overwrites prev */
			if (bpos_ge(prev->data->min_key, cur->data->min_key)) {		/* fully? */
				if (mustfix_fsck_err(trans, btree_node_topology_overwritten_by_next_node,
						     "btree node overwritten by next node%s", buf.buf)) {
					try(commit_topology_repair_log(trans));
					return bch_err_throw(c, topology_repair_drop_prev_node);
				}
			} else {
				if (mustfix_fsck_err(trans, btree_node_topology_bad_max_key,
						     "btree node with incorrect max_key%s", buf.buf)) {
					try(commit_topology_repair_log(trans));
					return set_node_max(trans, prev, bpos_predecessor(cur->data->min_key));
				}
			}
		} else {
			if (bpos_ge(expected_start, cur->data->max_key)) {		/* fully? */
				if (mustfix_fsck_err(trans, btree_node_topology_overwritten_by_prev_node,
						     "btree node overwritten by prev node%s", buf.buf)) {
					try(commit_topology_repair_log(trans));
					return bch_err_throw(c, topology_repair_drop_this_node);
				}
			} else {
				if (mustfix_fsck_err(trans, btree_node_topology_bad_min_key,
						     "btree node with incorrect min_key%s", buf.buf)) {
					try(commit_topology_repair_log(trans));
					return set_node_min(c, cur, expected_start);
				}
			}
		}
	}
fsck_err:
	return ret;
}

static int btree_check_root_boundaries(struct btree_trans *trans, struct btree *b)
{
	struct bch_fs *c = trans->c;
	int ret = 0;

	BUG_ON(b->key.k.type == KEY_TYPE_btree_ptr_v2 &&
	       !bpos_eq(bkey_i_to_btree_ptr_v2(&b->key)->v.min_key,
			b->data->min_key));

	CLASS(printbuf, buf)();
	prt_str(&buf, "  at ");
	bch2_btree_pos_to_text(&buf, c, b);

	if (mustfix_fsck_err_on(!bpos_eq(b->data->min_key, POS_MIN),
				trans, btree_node_topology_bad_root_min_key,
			     "btree root with incorrect min_key%s", buf.buf))
		try(set_node_min(c, b, POS_MIN));

	if (mustfix_fsck_err_on(!bpos_eq(b->data->max_key, SPOS_MAX),
				trans, btree_node_topology_bad_root_max_key,
			     "btree root with incorrect min_key%s", buf.buf))
		try(set_node_max(trans, b, SPOS_MAX));
fsck_err:
	return ret;
}

static int btree_repair_node_end(struct btree_trans *trans, struct btree *b, struct btree *child)
{
	struct bch_fs *c = trans->c;
	int ret = 0;

	if (bpos_eq(child->key.k.p, b->key.k.p))
		return 0;

	CLASS(printbuf, buf)();
	prt_printf(&buf, "\nat: ");
	bch2_btree_id_level_to_text(&buf, b->c.btree_id, b->c.level);
	prt_printf(&buf, "\nparent: ");
	bch2_bkey_val_to_text(&buf, c, bkey_i_to_s_c(&b->key));

	prt_str(&buf, "\nchild: ");
	bch2_bkey_val_to_text(&buf, c, bkey_i_to_s_c(&child->key));

	size_t nodes_found = 0;
	if (b->c.level == 1)
		try(bch2_get_scanned_nodes(c, b->c.btree_id, 0,
					   bpos_successor(child->key.k.p), b->key.k.p,
					   &buf, &nodes_found));

	if (mustfix_fsck_err(trans, btree_node_topology_bad_max_key,
			     "btree node with incorrect max_key%s", buf.buf)) {
		try(commit_topology_repair_log(trans));

		if (nodes_found)
			return bch_err_throw(c, topology_repair_did_fill_from_scan);
		else
			return set_node_max(trans, child, b->key.k.p);
	}
fsck_err:
	return ret;
}

static int bch2_btree_repair_topology_recurse(struct btree_trans *trans, struct btree *b)
{
	struct bch_fs *c = trans->c;
	struct btree_and_journal_iter iter;
	struct bkey_s_c k;
	struct btree *prev = NULL, *cur = NULL;
	bool have_child, new_pass = false;
	CLASS(printbuf, buf)();
	int ret = 0;

	if (!b->c.level)
		return 0;

	struct bkey_buf prev_k __cleanup(bch2_bkey_buf_exit);
	struct bkey_buf cur_k __cleanup(bch2_bkey_buf_exit);
	bch2_bkey_buf_init(&prev_k);
	bch2_bkey_buf_init(&cur_k);
again:
	cur = prev = NULL;
	have_child = new_pass = false;
	bch2_btree_and_journal_iter_init_node_iter(trans, &iter, b);
	iter.prefetch = true;

	while ((k = bch2_btree_and_journal_iter_peek(c, &iter)).k) {
		BUG_ON(bpos_lt(k.k->p, b->data->min_key));
		BUG_ON(bpos_gt(k.k->p, b->data->max_key));

		bch2_btree_and_journal_iter_advance(&iter);
		bch2_bkey_buf_reassemble(&cur_k, k);
		btree_node_mem_ptr_reload(cur_k.k, k.v);

		cur = bch2_btree_node_get_noiter(trans, cur_k.k,
					b->c.btree_id, b->c.level - 1,
					false);
		ret = PTR_ERR_OR_ZERO(cur);

		printbuf_reset(&buf);
		bch2_btree_id_level_to_text(&buf, b->c.btree_id, b->c.level - 1);
		prt_char(&buf, ' ');
		bch2_bkey_val_to_text(&buf, c, bkey_i_to_s_c(cur_k.k));

		if (bch2_err_matches(ret, EIO)) {
			bch2_btree_node_evict(trans, cur_k.k);
			cur = NULL;
			ret = bch2_journal_key_delete(c, b->c.btree_id,
						      b->c.level, cur_k.k->k.p);
			if (ret)
				break;
			continue;
		}

		bch_err_msg(c, ret, "getting btree node");
		if (ret)
			break;

		if (bch2_btree_node_is_stale(c, cur)) {
			bch_info(c, "btree node older than nodes found by scanning\n  %s", buf.buf);
			six_unlock_read(&cur->c.lock);
			bch2_btree_node_evict(trans, cur_k.k);
			ret = bch2_journal_key_delete(c, b->c.btree_id,
						      b->c.level, cur_k.k->k.p);
			cur = NULL;
			if (ret)
				break;
			continue;
		}

		ret = lockrestart_do(trans,
			btree_check_node_boundaries(trans, b, prev, cur));
		if (ret && !bch2_err_matches(ret, BCH_ERR_topology_repair))
			goto err;

		if (bch2_err_matches(ret, BCH_ERR_topology_repair_did_fill_from_scan)) {
			new_pass = true;
			ret = 0;
		}

		if (bch2_err_matches(ret, BCH_ERR_topology_repair_drop_this_node)) {
			six_unlock_read(&cur->c.lock);
			bch2_btree_node_evict(trans, cur_k.k);
			ret = bch2_journal_key_delete(c, b->c.btree_id,
						      b->c.level, cur_k.k->k.p);
			cur = NULL;
			if (ret)
				break;
			continue;
		}

		if (prev)
			six_unlock_read(&prev->c.lock);
		prev = NULL;

		if (bch2_err_matches(ret, BCH_ERR_topology_repair_drop_prev_node)) {
			bch_info(c, "dropped prev node");
			bch2_btree_node_evict(trans, prev_k.k);
			ret = bch2_journal_key_delete(c, b->c.btree_id,
						      b->c.level, prev_k.k->k.p);
			if (ret)
				break;

			/*
			 * cur is still read-locked, and goto again jumps over the
			 * unlock at the end of the loop; drop it here or it leaks
			 * (and later self-deadlocks a write lock on the same node).
			 */
			six_unlock_read(&cur->c.lock);
			cur = NULL;
			goto again;
		} else if (ret)
			break;

		prev = cur;
		cur = NULL;
		bch2_bkey_buf_copy(&prev_k, cur_k.k);
	}

	if (!ret && !IS_ERR_OR_NULL(prev)) {
		BUG_ON(cur);
		ret = lockrestart_do(trans,
			btree_repair_node_end(trans, b, prev));
		if (bch2_err_matches(ret, BCH_ERR_topology_repair_did_fill_from_scan)) {
			new_pass = true;
			ret = 0;
		}
	}

	if (!IS_ERR_OR_NULL(prev))
		six_unlock_read(&prev->c.lock);
	prev = NULL;
	if (!IS_ERR_OR_NULL(cur))
		six_unlock_read(&cur->c.lock);
	cur = NULL;

	if (ret)
		goto err;

	if (new_pass)
		goto again;

	bch2_btree_and_journal_iter_init_node_iter(trans, &iter, b);
	iter.prefetch = true;

	while ((k = bch2_btree_and_journal_iter_peek(c, &iter)).k) {
		bch2_bkey_buf_reassemble(&cur_k, k);
		btree_node_mem_ptr_reload(cur_k.k, k.v);
		bch2_btree_and_journal_iter_advance(&iter);

		cur = bch2_btree_node_get_noiter(trans, cur_k.k,
					b->c.btree_id, b->c.level - 1,
					false);
		ret = PTR_ERR_OR_ZERO(cur);

		bch_err_msg(c, ret, "getting btree node");
		if (ret)
			goto err;

		ret = bch2_btree_repair_topology_recurse(trans, cur);
		six_unlock_read(&cur->c.lock);
		cur = NULL;

		if (bch2_err_matches(ret, BCH_ERR_topology_repair_drop_this_node)) {
			bch2_btree_node_evict(trans, cur_k.k);
			ret = bch2_journal_key_delete(c, b->c.btree_id,
						      b->c.level, cur_k.k->k.p);
			new_pass = true;
		}

		if (ret)
			goto err;

		have_child = true;
	}

	printbuf_reset(&buf);
	bch2_btree_id_level_to_text(&buf, b->c.btree_id, b->c.level);
	prt_newline(&buf);
	bch2_bkey_val_to_text(&buf, c, bkey_i_to_s_c(&b->key));

	/*
	 * XXX: we're not passing the trans object here because we're not set up
	 * to handle a transaction restart - this code needs to be rewritten
	 * when we start doing online topology repair
	 */
	bch2_trans_unlock_long(trans);
	if (mustfix_fsck_err_on(!have_child,
			c, btree_node_topology_interior_node_empty,
			"empty interior btree node at %s", buf.buf))
		ret = bch_err_throw(c, topology_repair_drop_this_node);
err:
fsck_err:
	if (!IS_ERR_OR_NULL(prev))
		six_unlock_read(&prev->c.lock);
	if (!IS_ERR_OR_NULL(cur))
		six_unlock_read(&cur->c.lock);

	if (!ret && new_pass)
		goto again;

	BUG_ON(!ret && bch2_btree_node_check_topology(trans, b));

	if (!bch2_err_matches(ret, BCH_ERR_topology_repair))
		bch_err_fn(c, ret);
	return ret;
}

static int bch2_topology_check_root(struct btree_trans *trans, enum btree_id btree,
				    bool *reconstructed_root)
{
	struct bch_fs *c = trans->c;
	struct btree_root *r = bch2_btree_id_root(c, btree);
	int err = bch2_btree_id_root_error(c, btree);

	if (!err)
		return 0;

	CLASS(bch_log_msg, msg)(c);
	prt_printf(&msg.m, "btree root ");
	bch2_btree_id_to_text(&msg.m, btree);
	prt_printf(&msg.m, " unreadable: %s\n", bch2_err_str(err));

	if (!btree_id_recovers_from_scan(btree)) {
		r->alive = false;
		bch2_btree_id_root_set_error(c, btree, 0);
		bch2_btree_root_alloc_fake_trans(trans, btree, 0);
		*reconstructed_root = true;

		try(bch2_btree_lost_data(c, &msg.m, btree));
	} else {
		int ret = bch2_btree_has_scanned_nodes(c, btree, &msg.m);
		if (ret < 0)
			return ret;

		if (!ret) {
			msg.m.suppress = true;

			__ret_fsck_err(trans,
				       FSCK_CAN_FIX|(btree_id_can_reconstruct(btree) ? FSCK_AUTOFIX : 0),
				       btree_root_unreadable_and_scan_found_nothing,
				       "%sbtree node scan found no nodes, continue?", msg.m.buf);

			r->alive = false;
			bch2_btree_id_root_set_error(c, btree, 0);
			bch2_btree_root_alloc_fake_trans(trans, btree, 0);
			*reconstructed_root = true;
		} else {
			r->alive = false;
			bch2_btree_id_root_set_error(c, btree, 0);
			bch2_btree_root_alloc_fake_trans(trans, btree, 1);
			*reconstructed_root = true;

			bch2_shoot_down_journal_keys(c, btree, 1, BTREE_MAX_DEPTH, POS_MIN, SPOS_MAX);

			size_t nodes_found = 0;
			try(bch2_get_scanned_nodes(c, btree, 0, POS_MIN, SPOS_MAX, &msg.m, &nodes_found));
		}
	}

	/*
	 * Commit any fsck_err log entry queued above before returning: this
	 * runs pre-RW (so the commit is queued for journal replay), and the
	 * caller's per-btree loop starts each iteration with a bch2_trans_begin
	 * that would otherwise discard it.
	 */
	return bch2_trans_commit(trans, NULL, NULL, BCH_TRANS_COMMIT_no_enospc);
}

static void ratelimit_reset(struct ratelimit_state *rs)
{
	guard(raw_spinlock)(&rs->lock);
	atomic_set(&rs->rs_n_left, 0);
	atomic_set(&rs->missed, 0);
	rs->flags = 0;
	rs->begin = 0;
}

int bch2_check_topology(struct bch_fs *c)
{
	CLASS(btree_trans, trans)(c);

	bch2_trans_unlock_long(trans);

	for (unsigned i = 0; i < btree_id_nr_alive(c); i++) {
		bool reconstructed_root = false;
recover:
		try(lockrestart_do(trans, bch2_topology_check_root(trans, i, &reconstructed_root)));

		struct btree *b = bch2_btree_id_root_b(c, i);

		/*
		 * XXX: _nofail() locks need to die in a fire.
		 *
		 * Any nofail lock take is invisible to the cycle detector
		 * (the caller isn't recorded as a holder via any path), and
		 * in multithreaded mode nofail takers piling up on a single
		 * contended lock can blow through the wait_fifo cap and BUG.
		 * The cycle detector can't unwind deadlocks it can't see.
		 *
		 * The correct shape is may-fail: take the lock via a path,
		 * wrap in lockrestart_do, propagate transaction_restart up.
		 * But topology repair is old code that walks the btree with
		 * its own bch2_btree_and_journal_iter_* infrastructure, not
		 * via paths — and shoving a synthetic "I locked the root"
		 * path into that walking code doesn't work (the path's state
		 * isn't maintained by the walk, and verify_locks on mem_alloc
		 * slow paths blows up on the mismatch).
		 *
		 * Keeping nofail here is only OK because topology repair
		 * runs at mount/recovery time, before any worker threads are
		 * started, so there's physically nothing else that could be
		 * contending for these locks — wait_fifo can't fill up, cycle
		 * detection isn't needed.
		 *
		 * Proper fix is either (a) rewrite topology repair on top of
		 * paths, or (b) redo the whole nofail → may-fail plumbing so
		 * nofail callers can still get cycle-detector visibility via
		 * a thin "lock holder record" that isn't a full path. Either
		 * is bigger than this commit wants to be.
		 */
		trans->locking_hash_val = 0;
		trans->locking_root_id	= -1;
		btree_node_lock_nopath(trans, &b->c, SIX_LOCK_read, true, _THIS_IP_, false);
		int ret = btree_check_root_boundaries(trans, b) ?:
			  bch2_btree_repair_topology_recurse(trans, b);
		six_unlock_read(&b->c.lock);

		if (bch2_err_matches(ret, BCH_ERR_topology_repair_drop_this_node)) {
			/*
			 * Undo bch2_btree_set_root_inmem() before dropping the
			 * node: the cache state machine BUG_ONs on reclaiming a
			 * node that still says it's a root.
			 */
			scoped_guard(mutex, &c->btree.cache.root_lock)
				WRITE_ONCE(c->btree.cache.roots_b[i], 0);

			bch2_btree_node_set_permanent(c, b, false);

			/*
			 * Unhashing asserts SIX_LOCK_write even here, where
			 * recovery is the only thing running:
			 */
			trans->locking_hash_val = 0;
			trans->locking_root_id	= -1;
			btree_node_lock_nopath(trans, &b->c, SIX_LOCK_intent, true, _THIS_IP_, false);
			btree_node_lock_nopath(trans, &b->c, SIX_LOCK_write, true, _THIS_IP_, false);

			bch2_btree_node_transition_state(c, b, BTREE_NODE_CACHE_FREEABLE);

			six_unlock_write(&b->c.lock);
			six_unlock_intent(&b->c.lock);

			if (!reconstructed_root) {
				bch2_btree_id_root_set_error(c, i, -EIO);
				goto recover;
			}

			CLASS(printbuf, buf)();
			bch2_btree_id_to_text(&buf, i);
			bch_err(c, "empty btree root %s", buf.buf);
			bch2_btree_root_alloc_fake_trans(trans, i, 0);
			bch2_btree_id_root(c, i)->alive = false;
			ret = 0;
		}

		if (ret)
			return ret;
	}

	/*
	 * post topology repair there should be no errored nodes; reset
	 * ratelimiters so we see new unexpected errors
	 */
	ratelimit_reset(&c->btree.read_errors_soft);
	ratelimit_reset(&c->btree.read_errors_hard);

	return 0;
}
