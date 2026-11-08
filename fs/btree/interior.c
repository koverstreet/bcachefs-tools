// SPDX-License-Identifier: GPL-2.0

#include "bcachefs.h"

#include "alloc/buckets.h"
#include "alloc/disk_groups.h"
#include "alloc/foreground.h"
#include "alloc/replicas.h"

#include "btree/bkey_buf.h"
#include "btree/bkey_methods.h"
#include "btree/cache.h"
#include "btree/check.h"
#include "btree/update.h"
#include "btree/interior.h"
#include "btree/iter.h"
#include "btree/journal_overlay.h"
#include "btree/locking.h"
#include "btree/read.h"
#include "btree/sort.h"
#include "btree/write.h"

#include "data/extents.h"
#include "data/keylist.h"
#include "data/reconcile/trigger.h"
#include "data/write.h"

#include "init/error.h"
#include "init/fs.h"
#include "init/passes.h"

#include "journal/journal.h"
#include "journal/reclaim.h"

#include "sb/counters.h"
#include "sb/members.h"
#include "sb/io.h"

#include "util/enumerated_ref.h"

#include <linux/random.h>

void bch2_push_whiteout(struct btree *b, struct bpos pos)
{
	struct bkey_packed_padded k;

	BUG_ON(bch2_btree_keys_u64s_remaining(b) < BKEY_U64s);
	EBUG_ON(btree_node_just_written(b));

	if (!bch2_bkey_pack_pos(&k.k, pos, b)) {
		struct bkey *u = (void *) &k.k;

		bkey_init(u);
		u->p = pos;
	}

	k.k.needs_whiteout = true;

	b->whiteout_u64s += k.k.u64s;
	bkey_p_copy(unwritten_whiteouts_start(b), &k.k);
}

static const char * const bch2_btree_update_modes[] = {
#define x(t, ...) #t,
	BTREE_UPDATE_MODES()
#undef x
	NULL
};

/**
 * bch2_btree_node_format_fits - check if we could rewrite node with a new format
 *
 * @c:		filesystem handle
 * @b:		btree node to rewrite
 * @nr:		number of keys for new node (i.e. b->nr)
 * @new_f:	bkey format to translate keys to
 *
 * Returns: true if all re-packed keys will be able to fit in a new node.
 *
 * Assumes all keys will successfully pack with the new format.
 */
static bool bch2_btree_node_format_fits(struct bch_fs *c, struct btree *b,
				 struct btree_nr_keys nr,
				 struct bkey_format *new_f)
{
	size_t u64s = btree_node_u64s_with_format(nr, &b->format, new_f);

	return __vstruct_bytes(struct btree_node, u64s) < btree_buf_bytes(b);
}

/* Btree node freeing/allocation: */

static void __btree_node_free(struct btree_trans *trans, struct btree *b)
{
	struct bch_fs *c = trans->c;

	trace_btree_node(c, b, btree_node_free);

	BUG_ON(btree_node_write_blocked(b));
	BUG_ON(btree_node_need_write(b));
	BUG_ON(b == btree_node_root(c, b));
	BUG_ON(b->ob.nr);
	BUG_ON(!list_empty(&b->write_blocked));

	clear_btree_node_permanent(b);
	clear_btree_node_noevict(b);
}

void bch2_btree_node_free_inmem(struct btree_trans *trans,
				struct btree_path *path,
				struct btree *b)
{
	struct bch_fs *c = trans->c;

	BUG_ON(!btree_node_write_locked(path, b->c.level));

	__btree_node_free(trans, b);

	bch2_btree_node_transition_state(&c->btree.cache, b, BTREE_NODE_CACHE_FREEABLE);
	bch2_trans_node_forget(trans, b);
}

/*
 * Install a freshly-consumed prealloc node on @path at WRITE_LOCKED.
 *
 * Path takes its own intent+write recurses on top of the as-owned primary
 * refs from bch2_btree_node_mem_alloc — that keeps the path-state-vs-lock-
 * count invariant true.  six_lock_increment(SIX_LOCK_write) bumps both
 * write_lock_recurse and (via fallthrough) intent_lock_recurse — one call
 * gives both recurses.
 *
 * When the path eventually unlocks (trans teardown or explicit
 * bch2_path_put), btree_node_unlock for WRITE_LOCKED drops one intent +
 * one write balanced against the recurses we take here; the as-owned
 * refs survive and are dropped by bch2_btree_update_done (success) or
 * bch2_btree_reserve_put (err).
 */
void btree_path_take_new_node(struct btree_trans *trans,
			      struct btree_path *path,
			      struct btree *b)
{
	six_lock_increment(&b->c.lock, SIX_LOCK_write);
	mark_btree_node_locked(trans, path, b->c.level, BTREE_NODE_WRITE_LOCKED);
	bch2_btree_path_level_init(trans, path, b->c.level, b);
}

static bool can_use_btree_node(struct bch_fs *c,
			       struct disk_reservation *res,
			       unsigned target,
			       struct bkey_s_c k)
{
	if (!bch2_bkey_devs_rw(c, k))
		return false;

	if (target && !bch2_bkey_in_target(c, k, target))
		return false;

	unsigned durability = bch2_btree_ptr_durability(c, k).online;

	if (durability >= res->nr_replicas)
		return true;

	struct bch_devs_mask devs = target_rw_devs(c, BCH_DATA_btree, target);

	guard(rcu)();

	unsigned durability_available = 0, i;
	for_each_set_bit(i, devs.d, BCH_SB_MEMBERS_MAX) {
		struct bch_dev *ca = bch2_dev_rcu_noerror(c, i);
		if (ca)
			durability_available += bch2_dev_rw_durability(ca);
	}

	return durability >= durability_available;
}

static struct btree *__bch2_btree_node_alloc(struct btree_trans *trans,
					     struct disk_reservation *res,
					     bool interior_node,
					     struct alloc_request *req)
{
	struct bch_fs *c = trans->c;
	struct write_point *wp;
	struct btree *b;
	int ret;

	b = bch2_btree_node_mem_alloc(trans, interior_node);
	if (IS_ERR(b))
		return b;

	BUG_ON(b->ob.nr);
retry:
	ret = bch2_alloc_sectors_req(trans, req,
				     writepoint_ptr(&c->allocator.btree_write_point),
				     &wp);
	if (unlikely(ret))
		goto err;

	if (wp->sectors_free < btree_sectors(c)) {
		bch2_alloc_sectors_retire_short(c, wp, btree_sectors(c));
		goto retry;
	}

	mutex_lock(&c->btree.reserve_cache.lock);
	while (c->btree.reserve_cache.nr) {
		struct btree_alloc *a = c->btree.reserve_cache.data + --c->btree.reserve_cache.nr;

		/* check if it has sufficient durability */

		if (can_use_btree_node(c, res,
				       req->flags & BCH_WRITE_only_specified_devs ? req->target : 0,
				       bkey_i_to_s_c(&a->k))) {
			bkey_copy(&b->key, &a->k);
			b->ob = a->ob;
			mutex_unlock(&c->btree.reserve_cache.lock);
			goto out;
		}

		bch2_open_buckets_put(c, &a->ob);
	}
	mutex_unlock(&c->btree.reserve_cache.lock);

	bkey_btree_ptr_v2_init(&b->key);
	bch2_alloc_sectors_append_ptrs(c, wp, &b->key, btree_sectors(c), false);

	bch2_open_bucket_get(c, wp, &b->ob);
out:
	bch2_alloc_sectors_done(c, wp);
	/*
	 * Leave both intent and write held: nodes on the prealloc list stay
	 * locked (they aren't reachable to any other trans — not yet hashed
	 * in, or freshly hash-removed — so holding write blocks nothing).
	 * Consumers can use them without any lock-take at pop time.
	 */

	return b;
err:
	bch2_btree_node_transition_state(&c->btree.cache, b, BTREE_NODE_CACHE_FREEABLE);
	six_unlock_write(&b->c.lock);
	six_unlock_intent(&b->c.lock);
	return ERR_PTR(ret);
}

struct btree *bch2_btree_node_alloc(struct btree_update *as,
				    struct btree_trans *trans,
				    unsigned level)
{
	struct bch_fs *c = as->c;
	struct btree *b;
	struct prealloc_nodes *p = &as->prealloc_nodes[!!level];
	int ret;

	BUG_ON(level >= BTREE_MAX_DEPTH);
	BUG_ON(p->consumed >= p->nr);

	b = p->b[p->consumed++];

	/*
	 * @as keeps owning intent+write refs on @b until reserve_put runs at
	 * update_done/update_free time.  Consumers (this function's callers)
	 * additionally take their own intent+write recurses through the path
	 * machinery at the WRITE_LOCKED mark site.
	 */

	set_btree_node_accessed(b);

	bch2_bset_init_first(b, &b->data->keys);
	b->c.level	= level;
	b->c.btree_id	= as->btree_id;
	b->version_ondisk = c->sb.version;

	memset(&b->nr, 0, sizeof(b->nr));
	b->data->magic = cpu_to_le64(bset_magic(c));
	memset(&b->data->_ptr, 0, sizeof(b->data->_ptr));
	b->data->flags = 0;
	SET_BTREE_NODE_ID(b->data, as->btree_id);
	SET_BTREE_NODE_LEVEL(b->data, level);

	if (b->key.k.type == KEY_TYPE_btree_ptr_v2) {
		struct bkey_i_btree_ptr_v2 *bp = bkey_i_to_btree_ptr_v2(&b->key);

		bp->v.mem_ptr		= 0;
		bp->v.seq		= b->data->keys.seq;
		bp->v.sectors_written	= 0;
	}

	SET_BTREE_NODE_NEW_EXTENT_OVERWRITE(b->data, true);

	bch2_btree_build_aux_trees(b);

	ret = bch2_btree_node_transition_state(&c->btree.cache, b, btree_node_live_state(b));
	BUG_ON(ret);

	trace_btree_node(c, b, btree_node_alloc);
	bch2_increment_clock(c, btree_sectors(c), WRITE);
	return b;
}

static void btree_set_min(struct btree *b, struct bpos pos)
{
	if (b->key.k.type == KEY_TYPE_btree_ptr_v2)
		bkey_i_to_btree_ptr_v2(&b->key)->v.min_key = pos;
	b->data->min_key = pos;
}

static void btree_set_max(struct btree *b, struct bpos pos)
{
	b->key.k.p = pos;
	b->data->max_key = pos;
}

struct btree *bch2_btree_node_alloc_replacement(struct btree_update *as,
						struct btree_trans *trans,
						struct btree *b)
{
	struct btree *n = bch2_btree_node_alloc(as, trans, b->c.level);
	struct bkey_format format = bch2_btree_calc_format(b);

	/*
	 * The keys might expand with the new format - if they wouldn't fit in
	 * the btree node anymore, use the old format for now:
	 */
	if (!bch2_btree_node_format_fits(as->c, b, b->nr, &format))
		format = b->format;

	SET_BTREE_NODE_SEQ(n->data, BTREE_NODE_SEQ(b->data) + 1);

	btree_set_min(n, b->data->min_key);
	btree_set_max(n, b->data->max_key);

	n->data->format		= format;
	btree_node_set_format(n, format);

	bch2_btree_sort_into(as->c, n, b);

	btree_node_reset_sib_u64s(n);
	return n;
}

struct btree *__btree_root_alloc(struct btree_update *as,
				struct btree_trans *trans, unsigned level)
{
	struct btree *b = bch2_btree_node_alloc(as, trans, level);

	btree_set_min(b, POS_MIN);
	btree_set_max(b, SPOS_MAX);
	b->data->format = bch2_btree_calc_format(b);

	btree_node_set_format(b, b->data->format);
	bch2_btree_build_aux_trees(b);

	return b;
}

static void bch2_btree_reserve_put(struct btree_update *as, struct btree_trans *trans,
				   bool bailed)
{
	struct bch_fs *c = as->c;

	for (struct prealloc_nodes *p = as->prealloc_nodes;
	     p < as->prealloc_nodes + ARRAY_SIZE(as->prealloc_nodes);
	     p++) {
		for (unsigned i = 0; i < p->nr; i++) {
			struct btree *b = p->b[i];

			if (i >= p->consumed || bailed) {
				BUG_ON(b->written);

				mutex_lock(&c->btree.reserve_cache.lock);

				if (c->btree.reserve_cache.nr <
				    ARRAY_SIZE(c->btree.reserve_cache.data)) {
					struct btree_alloc *a =
						&c->btree.reserve_cache.data[c->btree.reserve_cache.nr++];

					a->ob = b->ob;
					b->ob.nr = 0;
					bkey_copy(&a->k, &b->key);
				} else {
					bch2_open_buckets_put(c, &b->ob);
				}

				mutex_unlock(&c->btree.reserve_cache.lock);

				clear_btree_node_need_write(b);
				__btree_node_free(trans, b);
				bch2_btree_node_transition_state(&c->btree.cache, b,
								 BTREE_NODE_CACHE_FREEABLE);
			}

			__bch2_btree_node_unlock_write(trans, b);
			six_unlock_intent(&b->c.lock);
		}

		p->nr = 0;
	}
}

static int bch2_btree_reserve_get(struct btree_trans *trans,
				  struct btree_update *as,
				  unsigned nr_nodes[2],
				  struct alloc_request *req)
{
	BUG_ON(nr_nodes[0] + nr_nodes[1] > BTREE_RESERVE_MAX);

	/*
	 * Protects reaping from the btree node cache and using the btree node
	 * open bucket reserve:
	 */
	try(bch2_btree_cache_cannibalize_lock(trans, req->cl));

	int ret = 0;
	for (unsigned interior = 0; interior < 2; interior++) {
		struct prealloc_nodes *p = as->prealloc_nodes + interior;

		while (p->nr < nr_nodes[interior]) {
			struct btree *b = __bch2_btree_node_alloc(trans, &as->disk_res,
								  interior, req);
			ret = PTR_ERR_OR_ZERO(b);
			if (ret)
				goto err;

			p->b[p->nr++] = b;
		}
	}
err:
	bch2_btree_cache_cannibalize_unlock(trans);
	return ret;
}

/* Asynchronous interior node update machinery */

void bch2_btree_update_free(struct btree_update *as, struct btree_trans *trans,
			    bool bailed)
{
	struct bch_fs *c = as->c;

	bch2_trans_unlock_write(trans);

	if (as->took_gc_lock)
		up_read(&c->gc.lock);
	as->took_gc_lock = false;

	bch2_journal_pin_drop(&c->journal, &as->journal);
	bch2_journal_pin_flush(&c->journal, &as->journal);
	bch2_disk_reservation_put(c, &as->disk_res);
	bch2_btree_reserve_put(as, trans, bailed);

	bch2_time_stats_update(&c->times[BCH_TIME_btree_interior_update_total],
			       as->start_time);

	guard(mutex)(&c->btree.interior_updates.lock);

	list_del(&as->unwritten_list);
	list_del(&as->list);

	closure_debug_destroy(&as->cl);
	mempool_free(as, &c->btree.interior_updates.pool);

	/*
	 * Have to do the wakeup with btree_interior_update_lock still held,
	 * since being on btree_interior_update_list is our ref on @c:
	 */
	closure_wake_up(&c->btree.interior_updates.wait);
}

void bch2_btree_update_add_key(btree_update_nodes *nodes,
			       unsigned level, struct bkey_i *k)
{
	BUG_ON(darray_make_room(nodes, 1));

	struct btree_update_node *n = &darray_top(*nodes);
	nodes->nr++;

	*n = (struct btree_update_node) { .level = level };
	bkey_copy(&n->key, k);
}

static void bch2_btree_update_add_node(struct bch_fs *c, btree_update_nodes *nodes, struct btree *b)
{
	BUG_ON(darray_make_room(nodes, 1));

	struct btree_update_node *n = &darray_top(*nodes);
	nodes->nr++;

	n->b		= b;
	n->level	= b->c.level;
	n->seq		= b->data->keys.seq;
	n->root		= b == btree_node_root(c, b);
	bkey_copy(&n->key, &b->key);
}

static bool btree_update_new_nodes_marked_sb(struct btree_update *as)
{
	darray_for_each(as->new_nodes, i)
		if (!bch2_dev_btree_bitmap_marked(as->c, bkey_i_to_s_c(&i->key)))
			return false;
	return true;
}

static void btree_update_new_nodes_mark_sb(struct btree_update *as)
{
	struct bch_fs *c = as->c;

	guard(mutex_noio)(&c->sb_lock);
	CLASS(sb_write, w)(c);
	darray_for_each(as->new_nodes, i)
		bch2_dev_btree_bitmap_mark_locked(c, bkey_i_to_s_c(&i->key), &w);
}

static bool bkey_has_reconcile(const struct bch_fs *c, struct bkey_s_c k)
{
	struct bkey_ptrs_c ptrs = bch2_bkey_ptrs_c(k);
	const union bch_extent_entry *entry;
	bkey_extent_entry_for_each(ptrs, entry)
		if (extent_entry_type(entry) == BCH_EXTENT_ENTRY_reconcile ||
		    (extent_entry_type(entry) == BCH_EXTENT_ENTRY_ptr &&
		     entry->ptr.dev == BCH_SB_MEMBER_INVALID))
			return true;
	return false;
}

/*
 * The transactional part of an interior btree node update, where we journal the
 * update we did to the interior node and update alloc info:
 */
static int btree_update_nodes_written_trans(struct btree_trans *trans,
					    struct btree_update *as)
{
	struct bch_fs *c = trans->c;
	struct bch_inode_opts opts;
	bch2_inode_opts_get(as->c, &opts, true);

	/*
	 * Our caller uses BCH_TRANS_COMMIT_no_check_rw, so after emergency
	 * read-only nothing else gates this path. Bail explicitly: running the
	 * alloc info triggers below for an interior update whose journal half
	 * won't land underflows bucket sector counts.
	 */
	try(bch2_journal_error(&c->journal));

	trans->journal_pin = &as->journal;

	darray_for_each(as->old_nodes, i) {
		try(bch2_key_trigger_old(trans, as->btree_id, i->level + 1, bkey_i_to_s_c(&i->key),
					 BTREE_TRIGGER_transactional));

		journal_entry_set(errptr_try(bch2_trans_jset_entry_alloc(trans,
									 jset_u64s(i->key.k.u64s))),
				  BCH_JSET_ENTRY_overwrite,
				  as->btree_id,
				  i->level + 1,
				  &i->key, i->key.k.u64s);
	}

	darray_for_each(as->new_nodes, i) {
		i->update_node_key = false;
		bch2_bkey_strip_reconcile(c, bkey_i_to_s(&i->key));

		try(bch2_bkey_set_needs_reconcile(trans, NULL, &opts, bkey_i_to_s(&i->key),
						  BKEY_BTREE_PTR_U64s_MAX,
						  SET_NEEDS_RECONCILE_foreground, 0));

		/*
		 * This is not strictly the best way of doing this, what we
		 * really want is a flag for 'did
		 * bch2_bkey_set_needs_reconcile() change anything, and do we
		 * need to update the node key'; there's no reason we couldn't
		 * be calling bch2_bkey_set_needs_reconcile() at node allocation
		 * time to better handle the case where we have to pad with
		 * invalid pointers because we don't currently have devices
		 * available to meet the desired replication level.
		 */

		if (bkey_has_reconcile(c, bkey_i_to_s_c(&i->key))) {
			CLASS(btree_iter_uninit, iter)(trans);
			int ret = bch2_btree_node_get_iter(trans, &iter, i->b);
			if (ret && ret != -BCH_ERR_btree_node_dying)
				return ret;
			if (!ret)
				i->update_node_key = true;
			else
				bch2_bkey_strip_reconcile(c, bkey_i_to_s(&i->key));
		}

		try(bch2_key_trigger_new(trans, as->btree_id, i->level + 1, bkey_i_to_s(&i->key),
					 BKEY_BTREE_PTR_U64s_MAX, BTREE_TRIGGER_transactional));

		if (!i->update_node_key || i->root) {
			journal_entry_set(errptr_try(bch2_trans_jset_entry_alloc(trans,
									jset_u64s(i->key.k.u64s))),
					  i->root
					  ? BCH_JSET_ENTRY_btree_root
					  : BCH_JSET_ENTRY_btree_keys,
					  as->btree_id,
					  i->root ? i->level : i->level + 1,
					  &i->key, i->key.k.u64s);
		} else {
			CLASS(btree_node_iter, parent_iter)(trans,
							    as->btree_id,
							    i->key.k.p,
							    0,
							    i->level + 1,
							    BTREE_ITER_intent);
			try(bch2_btree_iter_traverse(&parent_iter));
			/*
			 * XXX: we shouldn't be logging overwrites here, need a
			 * flag for that
			 */
			try(bch2_trans_update(trans, &parent_iter, &i->key, BTREE_TRIGGER_norun));
		}
	}

	return 0;
}

/* If the node has been reused, we might be reading uninitialized memory - that's fine: */
static noinline __no_kmsan_checks bool btree_node_seq_matches(struct btree *b, __le64 seq)
{
	/* b->data is not stable while read in flight */
	if (btree_node_read_in_flight(b))
		return false;

	struct btree_node *b_data = READ_ONCE(b->data);

	return (b_data ? b_data->keys.seq : 0) == seq;
}

static void btree_update_nodes_written(struct btree_update *as)
{
	struct bch_fs *c = as->c;
	CLASS(btree_trans, trans)(c);
	u64 journal_seq = 0;
	int ret = 0;

	if (!btree_update_new_nodes_marked_sb(as)) {
		bch2_trans_unlock_long(trans);
		btree_update_new_nodes_mark_sb(as);
	}

	/*
	 * Wait for any in flight writes to finish before we free the old nodes
	 * on disk. But we haven't pinned those old nodes in the btree cache,
	 * they might have already been evicted.
	 *
	 * The update we're completing deleted references to those nodes from the
	 * btree, so we know if they've been evicted they can't be pulled back in.
	 * We just have to check if the nodes we have pointers to are still those
	 * old nodes, and haven't been reused.
	 *
	 * This can't be done locklessly because the data buffer might have been
	 * vmalloc allocated, and they're not RCU freed. We also need the
	 * __no_kmsan_checks annotation because even with the btree node read
	 * lock, nothing tells us that the data buffer has been initialized (if
	 * the btree node has been reused for a different node, and the data
	 * buffer swapped for a new data buffer).
	 */
	darray_for_each(as->old_nodes, i) {
		bool seq_matches = false;

		/*
		 * i->b is a stale pointer: this update deleted the node, so
		 * btree_node_reclaim may have reused its memory — e.g. as
		 * another update's prealloc node, whose owner holds it
		 * intent+write while possibly blocked on the allocator. The
		 * open buckets the allocator is waiting on are released by
		 * this workqueue (bch2_open_bucket_put below), so sleeping on
		 * the node here closes the loop and deadlocks. Arm the
		 * identity check with the old key's hash: if we'd sleep on a
		 * node that's no longer ours, the lock attempt aborts.
		 *
		 * An abort means no wait is owed. Sustained write locks on an
		 * unhashed node come only from reclaim (which requires
		 * write_in_flight clear before grabbing) or the write
		 * completion path (which runs after the bio finished) — either
		 * way the IO this wait guards against is already done.
		 */
		ret = lockrestart_do(trans, ({
			btree_path_idx_t path_idx;
			int _ret = bch2_btree_node_lock_with_path(trans, &i->b->c,
								  SIX_LOCK_read,
								  btree_ptr_hash_val(&i->key),
								  &path_idx);
			if (!_ret) {
				seq_matches = btree_node_seq_matches(i->b, i->seq);
				bch2_btree_node_unlock_with_path(trans, path_idx,
								 i->b->c.level);
			}
			_ret;
		}));
		bch2_trans_unlock_long(trans);

		if (seq_matches)
			bch2_btree_node_wait_on_flag(i->b, BTREE_NODE_write_in_flight_inner);
	}

	/*
	 * We did an update to a parent node where the pointers we added pointed
	 * to child nodes that weren't written yet: now, the child nodes have
	 * been written so we can write out the update to the interior node.
	 */

	/*
	 * We can't call into journal reclaim here: we'd block on the journal
	 * reclaim lock, but we may need to release the open buckets we have
	 * pinned in order for other btree updates to make forward progress, and
	 * journal reclaim does btree updates when flushing bkey_cached entries,
	 * which may require allocations as well.
	 */

	bch2_trans_unlock(trans);
	/*
	 * btree_interior_update_commit_lock is needed for synchronization with
	 * btree_node_update_key(): having the lock be at the filesystem level
	 * sucks, we'll need to watch for contention
	 */
	scoped_guard(mutex, &c->btree.interior_updates.commit_lock) {
		ret = commit_do(trans, &as->disk_res, &journal_seq,
				BCH_WATERMARK_interior_updates|
				BCH_TRANS_COMMIT_no_enospc|
				BCH_TRANS_COMMIT_no_check_rw|
				BCH_TRANS_COMMIT_journal_reclaim,
				btree_update_nodes_written_trans(trans, as));
		bch2_fs_fatal_err_on(ret && !bch2_journal_error(&c->journal),
				     c, "%s", bch2_err_str(ret));
		/*
		 * Clear will_make_reachable while we still hold intent locks on
		 * all our new nodes, to avoid racing with
		 * btree_node_update_key():
		 */
		darray_for_each(as->new_nodes, i) {
			if (i->update_node_key)
				bkey_copy(&i->b->key, &i->key);

			if (i->b) {
				BUG_ON(i->b->will_make_reachable != (unsigned long) as);
				i->b->will_make_reachable = 0;
				clear_btree_node_will_make_reachable(i->b);
			}
		}
	}

	/*
	 * Ensure transaction is unlocked before using btree_node_lock_nopath()
	 * (the use of which is always suspect, we need to work on removing this
	 * in the future)
	 *
	 * It should be, but bch2_path_get_unlocked_mut() -> bch2_path_get()
	 * calls bch2_path_upgrade(), before we call path_make_mut(), so we may
	 * rarely end up with a locked path besides the one we have here:
	 */
	bch2_trans_unlock(trans);
	bch2_trans_begin(trans);

	/*
	 * We have to be careful because another thread might be getting ready
	 * to free as->b and calling btree_update_reparent() on us - we'll
	 * recheck under btree_update_lock below:
	 */
	struct btree *b = READ_ONCE(as->b);
	if (b) {
		/*
		 * @b is the node we did the final insert into:
		 *
		 * On failure to get a journal reservation, we still have to
		 * unblock the write and allow most of the write path to happen
		 * so that shutdown works, but the i->journal_seq mechanism
		 * won't work to prevent the btree write from being visible (we
		 * didn't get a journal sequence number) - instead
		 * __bch2_btree_node_write() doesn't do the actual write if
		 * we're in journal error state:
		 */
		lockrestart_do(trans, ({
			btree_path_idx_t path_idx;
			/*
			 * Unarmed take: this block owns clearing b's
			 * write_blocked (the list_del below), which is what
			 * makes b unreclaimable - so the reuse race can't
			 * reach it, and skipping on a spurious abort would
			 * leak write_blocked and leave b unwritable forever.
			 * The reparent race is handled by the as->b recheck
			 * under interior_updates.lock.
			 */
			int _ret = bch2_btree_node_lock_with_path(trans, &b->c,
							SIX_LOCK_intent, 0, &path_idx);
			if (!_ret) {
				struct btree_path *path = trans->paths + path_idx;

				_ret = bch2_btree_node_lock_write(trans, path, &b->c);
				if (_ret) {
					bch2_btree_node_unlock_with_path(trans, path_idx,
									 b->c.level);
				} else {
					bool do_pin;

					scoped_guard(mutex, &c->btree.interior_updates.lock) {
						list_del(&as->write_blocked_list);
						if (list_empty(&b->write_blocked))
							clear_btree_node_write_blocked(b);

						/*
						 * Node might have been freed, recheck under
						 * btree_interior_updates.lock; b's write lock
						 * keeps it valid past the mutex drop, so the
						 * journal pin work can happen outside the
						 * mutex.
						 */
						do_pin = as->b == b;
					}

					if (do_pin) {
						BUG_ON(!b->c.level);
						BUG_ON(!btree_node_dirty(b));

						if (!ret) {
							struct bset *last = btree_bset_last(b);

							last->journal_seq = cpu_to_le64(
									max(journal_seq,
									    le64_to_cpu(last->journal_seq)));

							bch2_btree_add_journal_pin(c, b, journal_seq);
						} else {
							/*
							 * If we didn't get a journal sequence
							 * number we can't write this btree node,
							 * because recovery won't know to ignore
							 * this write:
							 */
							set_btree_node_never_write(b);
						}
					}

					mark_btree_node_locked_noreset(path, b->c.level,
								       BTREE_NODE_INTENT_LOCKED);
					six_unlock_write(&b->c.lock);

					btree_node_write_if_need(trans, b, SIX_LOCK_intent);
					bch2_btree_node_unlock_with_path(trans, path_idx,
									 b->c.level);
				}
			}
			_ret;
		}));
	}

	bch2_journal_pin_drop(&c->journal, &as->journal);

	darray_for_each(as->new_nodes, i)
		if (i->b) {
			lockrestart_do(trans, ({
				btree_path_idx_t path_idx;
				/*
				 * Unarmed take: new nodes are dirty until
				 * written, hence unreclaimable - the reuse
				 * race can't reach them, and a spurious skip
				 * here would leave a node that never gets its
				 * write kicked.
				 */
				int _ret = bch2_btree_node_lock_with_path(trans, &i->b->c,
							SIX_LOCK_read, 0, &path_idx);
				if (!_ret) {
					btree_node_write_if_need(trans, i->b, SIX_LOCK_read);
					bch2_btree_node_unlock_with_path(trans, path_idx,
									 i->b->c.level);
				}
				_ret;
			}));
		}

	for (unsigned i = 0; i < as->nr_open_buckets; i++)
		bch2_open_bucket_put(c, c->allocator.open_buckets + as->open_buckets[i]);

	bch2_btree_update_free(as, trans, false);
}

static void btree_interior_update_work(struct work_struct *work)
{
	struct bch_fs *c =
		container_of(work, struct bch_fs, btree.interior_updates.work);
	struct btree_update *as;

	while (1) {
		scoped_guard(mutex, &c->btree.interior_updates.lock) {
			as = list_first_entry_or_null(&c->btree.interior_updates.unwritten,
						      struct btree_update, unwritten_list);
			if (as && !as->nodes_written)
				as = NULL;
		}

		if (!as)
			break;

		btree_update_nodes_written(as);
	}
}

static CLOSURE_CALLBACK(btree_update_set_nodes_written)
{
	closure_type(as, struct btree_update, cl);
	struct bch_fs *c = as->c;

	scoped_guard(mutex, &c->btree.interior_updates.lock)
		as->nodes_written = true;

	queue_work(c->btree.interior_updates.worker, &c->btree.interior_updates.work);
}

/*
 * We're updating @b with pointers to nodes that haven't finished writing yet:
 * block @b from being written until @as completes
 */
void btree_update_updated_node(struct btree_update *as, struct btree *b)
{
	struct bch_fs *c = as->c;

	BUG_ON(as->mode != BTREE_UPDATE_none);
	BUG_ON(as->update_level_end < b->c.level);
	BUG_ON(!btree_node_dirty(b));
	BUG_ON(!b->c.level);

	guard(mutex)(&c->btree.interior_updates.lock);
	list_add_tail(&as->unwritten_list, &c->btree.interior_updates.unwritten);

	as->mode	= BTREE_UPDATE_node;
	as->b		= b;
	as->update_level_end = b->c.level;

	set_btree_node_write_blocked(b);
	list_add(&as->write_blocked_list, &b->write_blocked);
}

static int bch2_update_reparent_journal_pin_flush(struct journal *j,
				struct journal_entry_pin *_pin, u64 seq)
{
	return 0;
}

static void btree_update_reparent(struct btree_update *as,
				  struct btree_update *child)
{
	struct bch_fs *c = as->c;

	lockdep_assert_held(&c->btree.interior_updates.lock);

	child->b = NULL;
	child->mode = BTREE_UPDATE_update;

	bch2_journal_pin_copy(&c->journal, &as->journal, &child->journal,
			      JOURNAL_PIN_TYPE_other,
			      bch2_update_reparent_journal_pin_flush);
}

static void btree_update_updated_root(struct btree_update *as, struct btree *b)
{
	struct bch_fs *c = as->c;

	BUG_ON(as->mode != BTREE_UPDATE_none);
	as->mode = BTREE_UPDATE_root;

	scoped_guard(mutex, &c->btree.interior_updates.lock)
		list_add_tail(&as->unwritten_list, &c->btree.interior_updates.unwritten);
}

/*
 * Stamp sectors_written on @b's btree_ptr_v2 key. Must be called before the
 * key is consumed (via bch2_keylist_add into parent_keys, or bch2_btree_set_root)
 * so that the key the parent commits and the entry in @as->new_nodes carry the
 * correct sectors_written.
 */
void bch2_btree_update_emit_new_node_key(struct btree_update *as, struct btree *b)
{
	struct bch_fs *c = as->c;

	if (b->key.k.type == KEY_TYPE_btree_ptr_v2) {
		unsigned bytes = vstruct_end(&b->data->keys) - (void *) b->data;
		unsigned sectors = round_up(bytes, block_bytes(c)) >> 9;

		bkey_i_to_btree_ptr_v2(&b->key)->v.sectors_written =
			cpu_to_le16(sectors);
	}
}

static void bch2_btree_update_get_open_buckets(struct btree_update *as, struct btree *b)
{
	while (b->ob.nr)
		as->open_buckets[as->nr_open_buckets++] =
			b->ob.v[--b->ob.nr];
}

/*
 * bch2_btree_update_write_new_node:
 *
 * Adds @b to @as's new_nodes list (so we capture root-status after
 * bch2_btree_set_root has run, and so btree_update_nodes_written can
 * iterate it for sb-mark, journal record, and will_make_reachable
 * cleanup), then hands @b off for writing: sets dirty + need_write
 * deferred from bch2_btree_node_alloc (so journal reclaim can't pick @b
 * up for flushing during construction), sets will_make_reachable to
 * gate any subsequent writes besides the first, then submits the first
 * write.
 *
 * Must be called after the parent commit has propagated @b's key (or
 * bch2_btree_set_root has installed @b as the root).  Anything that
 * needs @b->key in its committed-to-disk form (parent_keys / set_root)
 * must instead pair with bch2_btree_update_emit_new_node_key earlier.
 */
void bch2_btree_update_write_new_node(struct btree_update *as,
				      struct btree_trans *trans,
				      struct btree *b)
{
	struct bch_fs *c = as->c;

	bch2_btree_update_add_node(c, &as->new_nodes, b);

	closure_get(&as->cl);

	scoped_guard(mutex, &c->btree.interior_updates.lock) {
		BUG_ON(b->will_make_reachable);
		b->will_make_reachable = 1UL|(unsigned long) as;
		set_btree_node_will_make_reachable(b);
	}

	bch2_btree_node_set_dirty(c, b);
	set_btree_node_need_write(b);

	bch2_btree_update_get_open_buckets(as, b);
	bch2_btree_node_write_trans(trans, b, SIX_LOCK_write, 0);
}

static int bch2_btree_update_will_free_node_journal_pin_flush(struct journal *j,
				struct journal_entry_pin *_pin, u64 seq)
{
	return 0;
}

/*
 * @b is being split/rewritten: it may have pointers to not-yet-written btree
 * nodes and thus outstanding btree_updates - redirect @b's
 * btree_updates to point to this btree_update:
 */
void bch2_btree_interior_update_will_free_node(struct btree_update *as,
					       struct btree *b)
{
	struct bch_fs *c = as->c;
	struct btree_update *p, *n;
	struct btree_write *w;

	set_btree_node_dying(b);

	if (btree_node_fake(b))
		return;

	/*
	 * Does this node have any btree_update operations preventing
	 * it from being written?
	 *
	 * If so, redirect them to point to this btree_update: we can
	 * write out our new nodes, but we won't make them visible until those
	 * operations complete
	 */
	scoped_guard(mutex, &c->btree.interior_updates.lock)
		list_for_each_entry_safe(p, n, &b->write_blocked, write_blocked_list) {
			list_del_init(&p->write_blocked_list);
			btree_update_reparent(as, p);

			/*
			 * for flush_held_btree_writes() waiting on updates to flush or
			 * nodes to be writeable:
			 */
			closure_wake_up(&c->btree.interior_updates.wait);
		}

	clear_btree_node_dirty(b);
	clear_btree_node_need_write(b);
	clear_btree_node_write_blocked(b);

	/*
	 * Does this node have unwritten data that has a pin on the journal?
	 *
	 * If so, transfer that pin to the btree_update operation -
	 * note that if we're freeing multiple nodes, we only need to keep the
	 * oldest pin of any of the nodes we're freeing. We'll release the pin
	 * when the new nodes are persistent and reachable on disk:
	 */
	w = btree_current_write(b);
	bch2_journal_pin_copy(&c->journal, &as->journal, &w->journal,
			      JOURNAL_PIN_TYPE_other,
			      bch2_btree_update_will_free_node_journal_pin_flush);
	bch2_journal_pin_drop(&c->journal, &w->journal);

	w = btree_prev_write(b);
	bch2_journal_pin_copy(&c->journal, &as->journal, &w->journal,
			      JOURNAL_PIN_TYPE_other,
			      bch2_btree_update_will_free_node_journal_pin_flush);
	bch2_journal_pin_drop(&c->journal, &w->journal);

	bch2_btree_update_add_node(c, &as->old_nodes, b);
}

void bch2_btree_update_done(struct btree_update *as, struct btree_trans *trans)
{
	struct bch_fs *c = as->c;
	u64 start_time = as->start_time;

	bch2_trans_unlock_write(trans);

	BUG_ON(as->mode == BTREE_UPDATE_none);

	if (as->took_gc_lock)
		up_read(&as->c->gc.lock);
	as->took_gc_lock = false;

	bch2_btree_reserve_put(as, trans, false);

	continue_at(&as->cl, btree_update_set_nodes_written,
		    as->c->btree.interior_updates.worker);

	bch2_time_stats_update(&c->times[BCH_TIME_btree_interior_update_foreground],
			       start_time);
}

static const char * const btree_node_reawrite_reason_strs[] = {
#define x(n, ...)	#n,
	BTREE_NODE_REWRITE_REASON()
#undef x
	NULL,
};

struct btree_update *
bch2_btree_update_start(struct btree_trans *trans, btree_path_idx_t path_idx,
			unsigned level_start, bool split,
			unsigned target,
			enum bch_trans_commit_flags commit_flags,
			enum bch_write_flags write_flags)
{
	struct bch_fs *c = trans->c;
	/*
	 * Only valid until bch2_btree_reserve_get(): the allocator's btree
	 * iterators can grow and thus reallocate trans->paths
	 */
	struct btree_path *path = trans->paths + path_idx;
	struct btree_update *as;
	u64 start_time = local_clock();
	int disk_res_flags = (commit_flags & BCH_TRANS_COMMIT_no_enospc)
		? BCH_DISK_RESERVATION_NOFAIL : 0;
	unsigned nr_nodes[2] = { 0, 0 };
	unsigned level_end = level_start;
	enum bch_watermark watermark = commit_flags & BCH_WATERMARK_MASK;
	struct alloc_request *req __free(alloc_request_put) = NULL;
	int ret = 0;
	u32 restart_count = trans->restart_count;

	bch2_trans_verify_paths(trans);

	BUG_ON(!path->should_be_locked);

	if (watermark == BCH_WATERMARK_stripe) {
		watermark = BCH_WATERMARK_normal;
		commit_flags &= ~BCH_WATERMARK_MASK;
		commit_flags |= watermark;
	}

	/*
	 * Going read-only, every interior update is part of the final journal
	 * flush, which can't finish until it does - and discards, the only
	 * thing that would free buckets, stopped with writes. Below the
	 * reclaim watermark a node allocation can wait on the btree reserve
	 * forever (unmount hanging in __bch2_wait_on_allocator()):
	 */
	if (unlikely(test_bit(BCH_FS_going_ro, &c->flags)) &&
	    watermark < BCH_WATERMARK_reclaim) {
		watermark = BCH_WATERMARK_reclaim;
		commit_flags &= ~BCH_WATERMARK_MASK;
		commit_flags |= watermark;
	}

	if (watermark < BCH_WATERMARK_reclaim &&
	    journal_low_on_space(&c->journal)) {
		if (commit_flags & BCH_TRANS_COMMIT_journal_reclaim)
			return ERR_PTR(-BCH_ERR_journal_reclaim_would_deadlock);

		/*
		 * Or until the journal errors out: a halted journal stays low on
		 * space for good, and the bch2_journal_error() check below
		 * returns the error (tools#971 - rewrite workers queued after a
		 * failed mount waited here forever, holding up bch2_fs_stop()):
		 */
		ret = drop_locks_do(trans,
			({ trans_wait_event(trans, &c->journal.async_wait,
					    !journal_low_on_space(&c->journal) ||
					    bch2_journal_error(&c->journal)); 0; }));
		if (ret)
			return ERR_PTR(ret);
	}

	while (1) {
		nr_nodes[!!level_end] += 1 + split;
		level_end++;

		ret = bch2_btree_path_upgrade(trans, path, level_end + 1);
		if (ret)
			return ERR_PTR(ret);

		if (!btree_path_node(path, level_end)) {
			/* Allocating new root? */
			nr_nodes[1] += split;
			level_end = BTREE_MAX_DEPTH;
			break;
		}

		/*
		 * Always check for space for two keys, even if we won't have to
		 * split at prior level - it might have been a merge instead:
		 */
		if (bch2_btree_node_insert_fits(path->l[level_end].b,
						BKEY_BTREE_PTR_U64s_MAX * 2))
			break;

		split = path->l[level_end].b->nr.live_u64s > BTREE_SPLIT_THRESHOLD(c);
	}

	if (!down_read_trylock(&c->gc.lock)) {
		ret = drop_locks_do(trans, (down_read(&c->gc.lock), 0));
		if (ret) {
			up_read(&c->gc.lock);
			return ERR_PTR(ret);
		}
	}

	as = mempool_alloc(&c->btree.interior_updates.pool, GFP_NOIO);
	memset(as, 0, sizeof(*as));
	closure_init(&as->cl, NULL);
	as->c			= c;
	as->disk_res.c		= c;
	as->start_time		= start_time;
	as->ip_started		= _RET_IP_;
	as->mode		= BTREE_UPDATE_none;
	as->flags		= commit_flags;
	as->took_gc_lock	= true;
	as->btree_id		= path->btree_id;
	as->update_level_start	= level_start;
	as->update_level_end	= level_end;
	INIT_LIST_HEAD(&as->list);
	INIT_LIST_HEAD(&as->unwritten_list);
	INIT_LIST_HEAD(&as->write_blocked_list);
	darray_init(&as->old_nodes);
	darray_init(&as->new_nodes);
	bch2_keylist_init(&as->parent_keys, as->inline_keys);

	scoped_guard(mutex, &c->btree.interior_updates.lock)
		list_add_tail(&as->list, &c->btree.interior_updates.list);

	struct btree *b = btree_path_node(path, path->level);
	as->node_start	= b->data->min_key;
	as->node_end	= b->data->max_key;
	as->node_needed_rewrite = btree_node_rewrite_reason(b);
	as->node_written = b->written;
	as->node_sectors = btree_buf_bytes(b) >> 9;
	as->node_remaining = __bch2_btree_u64s_remaining(b,
				btree_bkey_last(b, bset_tree_last(b)));

	/*
	 * We don't want to allocate if we're in an error state, that can cause
	 * deadlock on emergency shutdown due to open buckets getting stuck in
	 * the btree_reserve_cache after allocator shutdown has cleared it out.
	 * This check needs to come after adding us to the btree_interior_update
	 * list but before calling bch2_btree_reserve_get, to synchronize with
	 * __bch2_fs_read_only().
	 */
	ret = bch2_journal_error(&c->journal);
	if (ret)
		goto err;

	ret = bch2_disk_reservation_add(c, &as->disk_res,
			(nr_nodes[0] + nr_nodes[1]) * btree_sectors(c),
			READ_ONCE(c->opts.metadata_replicas),
			disk_res_flags);
	if (ret)
		goto err;

	struct bch_devs_list devs_have = (struct bch_devs_list) { 0 };
	req = alloc_request_get(trans,
				target ?:
				c->opts.metadata_target ?:
				c->opts.foreground_target,
				false,
				&devs_have,
				as->disk_res.nr_replicas,
				as->disk_res.nr_replicas,
				watermark,
				write_flags,
				NULL);
	ret = PTR_ERR_OR_ZERO(req);
	if (ret)
		goto err;

	ret = bch2_btree_reserve_get(trans, as, nr_nodes, req);
	if (bch2_err_matches(ret, EAGAIN) ||
	    bch2_err_matches(ret, ENOMEM)) {
		/*
		 * XXX: this should probably be a separate BTREE_INSERT_NONBLOCK
		 * flag
		 */
		if (bch2_err_matches(ret, EAGAIN) &&
		    (commit_flags & BCH_TRANS_COMMIT_journal_reclaim) &&
		    watermark < BCH_WATERMARK_reclaim) {
			ret = bch_err_throw(c, journal_reclaim_would_deadlock);
			goto err;
		}

		CLASS(closure_stack, cl)();
		req->cl = &cl;

		do {
			ret = bch2_btree_reserve_get(trans, as, nr_nodes, req);
			if (!bch2_err_matches(ret, BCH_ERR_operation_blocked))
				break;
			ret = bch2_wait_on_allocator(trans, req, ret, &cl);
		} while (!ret);

		/*
		 * Don't block with btree locks held
		 *
		 * It would be nice if we could remove closures from waitlists
		 * without waking up the waitlist:
		 */
		if (closure_nr_remaining(&cl) > 1)
			bch2_trans_unlock(trans);
	}

	if (ret) {
		event_inc_trace(c, btree_reserve_get_fail, buf, ({
			prt_printf(&buf, "%s\n", trans->fn);
			prt_printf(&buf, "need %u ret %s\n",
				   nr_nodes[0] + nr_nodes[1], bch2_err_str(ret));
		}));
		goto err;
	}

	ret = bch2_trans_relock(trans);
	if (ret)
		goto err;

	bch2_trans_verify_not_restarted(trans, restart_count);
	return as;
err:
	bch2_btree_update_free(as, trans, true);
	if (!bch2_err_matches(ret, ENOSPC) &&
	    !bch2_err_matches(ret, EROFS) &&
	    ret != -BCH_ERR_journal_reclaim_would_deadlock &&
	    ret != -BCH_ERR_journal_shutdown)
		bch_err_fn_ratelimited(c, ret);
	return ERR_PTR(ret);
}

/* Btree root updates: */

static void bch2_btree_set_root_inmem(struct bch_fs *c, struct btree *b)
{
	/*
	 * Root nodes cannot be reaped. The flag (rather than off-list
	 * placement) is the mechanism: the reclaim path checks
	 * btree_node_permanent and skips. Roots stay on bc->list,
	 * counted normally; cache_exit's teardown sweep finds them.
	 */
	scoped_guard(mutex_noio, &c->btree.cache.lock)
		set_btree_node_permanent(b);

	scoped_guard(mutex, &c->btree.cache.root_lock)
		WRITE_ONCE(c->btree.cache.roots_b[b->c.btree_id],
			   bch2_btree_root_pack(b));

	bch2_recalc_btree_reserve(c);
}

void bch2_btree_set_root(struct btree_update *as,
			 struct btree_trans *trans,
			 struct btree_path *path,
			 struct btree *b)
{
	struct bch_fs *c = as->c;

	trace_btree_node(c, b, btree_node_set_root);

	struct btree *old = btree_node_root(c, b);

	BUG_ON(!btree_node_write_locked(path, old->c.level));

	bch2_btree_set_root_inmem(c, b);

	btree_update_updated_root(as, b);
}

int bch2_btree_node_get_iter(struct btree_trans *trans, struct btree_iter *iter, struct btree *b)
{
	bch2_trans_node_iter_init(trans, iter, b->c.btree_id, b->key.k.p,
				  BTREE_MAX_DEPTH, b->c.level,
				  BTREE_ITER_intent);
	try(bch2_btree_iter_traverse(iter));

	/* has node been freed? */
	if (btree_iter_path(trans, iter)->l[b->c.level].b != b) {
		/* node has been freed: */
		BUG_ON(!btree_node_dying(b));
		return bch_err_throw(trans->c, btree_node_dying);
	}

	BUG_ON(!btree_node_hashed(b));
	return 0;
}

/* Init code: */

/*
 * Only for filesystem bringup, when first reading the btree roots or allocating
 * btree roots when initializing a new filesystem:
 */
void bch2_btree_set_root_for_read(struct bch_fs *c, struct btree *b)
{
	BUG_ON(btree_node_root(c, b));

	bch2_btree_set_root_inmem(c, b);
}

int bch2_btree_root_alloc_fake_trans(struct btree_trans *trans, enum btree_id id, unsigned level)
{
	struct bch_fs *c = trans->c;
	struct btree *b;
	int ret;

	CLASS(closure_stack, cl)();

	do {
		ret = bch2_btree_cache_cannibalize_lock(trans, &cl);
		closure_sync(&cl);
	} while (ret);

	b = bch2_btree_node_mem_alloc(trans, false);
	bch2_btree_cache_cannibalize_unlock(trans);

	ret = PTR_ERR_OR_ZERO(b);
	if (ret)
		return ret;

	set_btree_node_fake(b);
	set_btree_node_need_rewrite(b);
	b->c.level	= level;
	b->c.btree_id	= id;

	bkey_btree_ptr_init(&b->key);
	b->key.k.p = SPOS_MAX;
	*((u64 *) bkey_i_to_btree_ptr(&b->key)->v.start) = U64_MAX - id;

	bch2_bset_init_first(b, &b->data->keys);
	bch2_btree_build_aux_trees(b);

	b->data->flags = 0;
	btree_set_min(b, POS_MIN);
	btree_set_max(b, SPOS_MAX);
	b->data->format = bch2_btree_calc_format(b);
	btree_node_set_format(b, b->data->format);

	ret = bch2_btree_node_transition_state(&c->btree.cache, b, btree_node_live_state(b));
	BUG_ON(ret);

	bch2_btree_set_root_inmem(c, b);

	six_unlock_write(&b->c.lock);
	six_unlock_intent(&b->c.lock);
	return 0;
}

void bch2_btree_root_alloc_fake(struct bch_fs *c, enum btree_id id, unsigned level)
{
	CLASS(btree_trans, trans)(c);
	lockrestart_do(trans, bch2_btree_root_alloc_fake_trans(trans, id, level));
}

__cold void bch2_btree_update_to_text(struct printbuf *out, struct btree_update *as)
{
	prt_printf(out, "%ps: ", (void *) as->ip_started);
	bch2_trans_commit_flags_to_text(out, as->flags);

	prt_str(out, " ");
	bch2_btree_id_to_text(out, as->btree_id);
	prt_printf(out, " l=%u-%u ",
		   as->update_level_start,
		   as->update_level_end);
	bch2_bpos_to_text(out, as->node_start);
	prt_char(out, ' ');
	bch2_bpos_to_text(out, as->node_end);
	prt_printf(out, "\nwritten %u/%u u64s_remaining %u need_rewrite %s",
		   as->node_written,
		   as->node_sectors,
		   as->node_remaining,
		   btree_node_reawrite_reason_strs[as->node_needed_rewrite]);

	prt_printf(out, "\nmode=%s nodes_written=%u cl.remaining=%u journal_seq=%llu\n",
		   bch2_btree_update_modes[as->mode],
		   as->nodes_written,
		   closure_nr_remaining(&as->cl),
		   as->journal.seq);
}

__cold void bch2_btree_updates_to_text(struct printbuf *out, struct bch_fs *c)
{
	struct btree_update *as;

	guard(mutex)(&c->btree.interior_updates.lock);
	list_for_each_entry(as, &c->btree.interior_updates.list, list)
		bch2_btree_update_to_text(out, as);
}

bool bch2_btree_interior_updates_flush(struct bch_fs *c)
{
	bool ret = bch2_btree_interior_updates_pending(c);

	if (ret)
		closure_wait_event(&c->btree.interior_updates.wait,
				   !bch2_btree_interior_updates_pending(c));
	return ret;
}

void bch2_journal_entry_to_btree_root(struct bch_fs *c, struct jset_entry *entry)
{
	struct btree_root *r = bch2_btree_id_root(c, entry->btree_id);

	/*
	 * The on-disk root fields (key/level/alive) are owned by root_lock:
	 * that's what bch2_btree_set_root_inmem() takes for ->b, and what the
	 * journal replay path takes for ->key. Taking interior_updates.lock
	 * here was incidental (roots are interior-update machinery), and it
	 * dragged interior_updates.lock into the journal write path under
	 * sb_lock — closing a lockdep cycle with pin_resize_lock.
	 */
	guard(mutex)(&c->btree.cache.root_lock);

	r->level = entry->level;
	r->alive = true;
	bkey_copy(&r->key, (struct bkey_i *) entry->start);
}

static __cold void bch2_btree_alloc_to_text(struct printbuf *out,
				     struct bch_fs *c,
				     struct btree_alloc *a)
{
	guard(printbuf_indent)(out);
	bch2_bkey_val_to_text(out, c, bkey_i_to_s_c(&a->k));
	prt_newline(out);

	struct open_bucket *ob;
	unsigned i;
	open_bucket_for_each(c, &a->ob, ob, i)
		bch2_open_bucket_to_text(out, c, ob);
}

__cold void bch2_btree_reserve_cache_to_text(struct printbuf *out, struct bch_fs *c)
{
	for (unsigned i = 0; i < c->btree.reserve_cache.nr; i++)
		bch2_btree_alloc_to_text(out, c, &c->btree.reserve_cache.data[i]);
}

void bch2_fs_btree_interior_update_init_early(struct bch_fs *c)
{
	INIT_LIST_HEAD(&c->btree.interior_updates.list);
	INIT_LIST_HEAD(&c->btree.interior_updates.unwritten);
	mutex_init(&c->btree.interior_updates.lock);
	mutex_init(&c->btree.interior_updates.commit_lock);
	INIT_WORK(&c->btree.interior_updates.work, btree_interior_update_work);
}
