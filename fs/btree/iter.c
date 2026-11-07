// SPDX-License-Identifier: GPL-2.0

/* DOC_LATEX(btree-programmer-interface)
 *
 * This section describes the btree interface from a programmer's perspective:
 * how to look up keys, iterate, and perform updates.
 *
 * \paragraph{Btrees and keys}
 *
 * Keys are indexed in a small number of btrees: one for extents, another for
 * inodes, another for directory entries, and so on. New btree types can be
 * added for new features without on-disk format changes---many features were
 * added this way (e.g.\ erasure coding stripes, backpointers, accounting).
 *
 * The btree interface is iteration-oriented rather than lookup-oriented.
 * Lookup is not exposed as a primitive because most usage involves iterating
 * from a given position. The fundamental operation is: position an iterator,
 * then peek at or advance through keys.
 *
 * \paragraph{Transactions}
 *
 * All btree operations go through a transaction (\texttt{struct btree\_trans}).
 * A transaction provides:
 *
 * \begin{itemize}
 * \item Atomic multi-key updates across any combination of btrees
 * \item Automatic lock management and deadlock avoidance
 * \item Restart handling when locks are contended
 * \end{itemize}
 *
 * Transactions are allocated with \texttt{bch2\_trans\_get()} and released with
 * \texttt{bch2\_trans\_put()}. Within a transaction, iterators are created with
 * \texttt{bch2\_trans\_iter\_init()} specifying the btree ID and starting
 * position.
 *
 * \paragraph{Basic iteration}
 *
 * The core iteration pattern:
 *
 * \begin{verbatim}
 *     struct btree_iter iter;
 *     struct bkey_s_c k;
 *
 *     bch2_trans_iter_init(trans, &iter, BTREE_ID_extents,
 *                          POS(inode, offset), 0);
 *
 *     while ((k = bch2_btree_iter_peek(&iter)).k) {
 *         // process key
 *         bch2_btree_iter_advance(&iter);
 *     }
 *
 *     bch2_trans_iter_exit(trans, &iter);
 * \end{verbatim}
 *
 * The convenience macro \texttt{for\_each\_btree\_key()} wraps this pattern:
 *
 * \begin{verbatim}
 *     for_each_btree_key(trans, iter, BTREE_ID_extents,
 *                        POS(inode, 0), 0, k, ret)
 *         printk("extent at %llu:%llu\n",
 *                k.k->p.inode, k.k->p.offset);
 * \end{verbatim}
 *
 * \paragraph{Lookup}
 *
 * Lookup can be implemented via iteration: position an iterator, peek, and
 * check if the returned key matches:
 *
 * \begin{verbatim}
 *     bch2_trans_iter_init(trans, &iter, BTREE_ID_inodes,
 *                          POS(inum, 0), 0);
 *     k = bch2_btree_iter_peek_slot(&iter);
 *     if (k.k && k.k->type == KEY_TYPE_inode)
 *         // found it
 * \end{verbatim}
 *
 * The \texttt{peek\_slot()} variant returns a key at exactly the requested
 * position, synthesizing a deleted key if no key exists there. This is useful
 * for checking whether a specific slot is occupied.
 *
 * \paragraph{Updates}
 *
 * Updates are staged in the transaction and committed atomically:
 *
 * \begin{verbatim}
 *     struct bkey_i_inode *inode = ...;
 *
 *     ret = bch2_trans_update(trans, &iter, &inode->k_i, 0);
 *     if (ret)
 *         return ret;
 *
 *     ret = bch2_trans_commit(trans, NULL, NULL, 0);
 * \end{verbatim}
 *
 * Multiple updates can be staged before a single commit, and they will all
 * be applied atomically. Updates to different btrees within the same
 * transaction are also atomic.
 *
 * \paragraph{Restarts}
 *
 * Transactions may need to restart due to lock contention or other conflicts.
 * The standard pattern uses \texttt{lockrestart\_do()}:
 *
 * \begin{verbatim}
 *     ret = lockrestart_do(trans,
 *         do_something(trans) ?:
 *         bch2_trans_commit(trans, NULL, NULL, 0));
 * \end{verbatim}
 *
 * This automatically retries the operation if a restart is needed. All code
 * within the retry block must be idempotent---it may execute multiple times.
 */

/* DOC(btree-iterators)
 *
 * A btree iterator (`struct btree_iter`) maintains a full path from the root
 * of a btree down to a specific key in a leaf node. At each level, the
 * iterator holds a pointer to the btree node, a node-level iterator (which
 * merges across the multiple sorted bsets within that node), and a lock
 * sequence number for efficient relock-after-restart.
 *
 * The fundamental invariant is that `iter.pos` must always be consistent with
 * the node iterators: at every level, the node iterator points to the first
 * key >= `iter.pos`, and the previous key compares strictly less. This is the
 * correct position for inserting a new key at `iter.pos`.
 *
 * Iterators support several modes: `peek()` returns the next key and advances
 * `iter.pos` to match; `peek_slot()` returns the key at exactly `iter.pos`
 * (synthesizing a deleted key for empty slots); `peek_prev()` iterates
 * backwards. Extent iterators have special semantics: extents are indexed by
 * their end position, so the first extent covering a range starting at
 * `iter.pos` is found by searching for the first key strictly greater than
 * `iter.pos`. When iterating over extents by slots, holes between extents are
 * synthesized, guaranteeing that the returned keys exactly cover the keyspace
 * with monotonically increasing positions.
 *
 * Multiple iterators within a transaction coexist without invalidating each
 * other. On transaction restart, each iterator's saved position allows it to
 * resume from where it left off.
 */
#include "bcachefs.h"

#include "alloc/replicas.h"

#include "btree/bkey_methods.h"
#include "btree/bkey_buf.h"
#include "btree/cache.h"
#include "btree/interior.h"
#include "btree/iter.h"
#include "btree/journal_overlay.h"
#include "btree/key_cache.h"
#include "btree/locking.h"
#include "btree/update.h"

#include "data/extents.h"

#include "debug/debug.h"

#include "init/error.h"
#include "init/fs.h"

#include "journal/journal.h"
#include "journal/validate.h"

#include "sb/counters.h"

#include "snapshots/snapshot.h"

#include <linux/random.h>
#include <linux/prefetch.h>

static inline unsigned long btree_iter_ip_allocated(struct btree_iter *iter)
{
#ifdef TRACK_PATH_ALLOCATED
	return iter->ip_allocated;
#else
	return 0;
#endif
}

static inline struct bpos bkey_successor(struct btree_iter *iter, struct bpos p)
{
	/* Are we iterating over keys in all snapshots? */
	return iter->flags & BTREE_ITER_all_snapshots
		? bpos_successor(p)
		: bpos_with_snapshot(bpos_nosnap_successor(p), iter->snapshot);
}

static inline struct bpos bkey_predecessor(struct btree_iter *iter, struct bpos p)
{
	/* Are we iterating over keys in all snapshots? */
	if (iter->flags & BTREE_ITER_all_snapshots) {
		p = bpos_predecessor(p);
	} else {
		p = bpos_nosnap_predecessor(p);
		p.snapshot = iter->snapshot;
	}

	return p;
}

static inline struct bpos btree_iter_search_key(struct btree_iter *iter)
{
	struct bpos pos = iter->pos;

	if ((iter->flags & BTREE_ITER_is_extents) &&
	    !bkey_eq(pos, POS_MAX))
		pos = bkey_successor(iter, pos);
	return pos;
}

/* Debug: */

static void __bch2_btree_iter_verify(struct btree_iter *iter)
{
	struct btree_trans *trans = iter->trans;

	BUG_ON(!!(iter->flags & BTREE_ITER_cached) != btree_iter_path(trans, iter)->cached);

	BUG_ON((iter->flags & BTREE_ITER_is_extents) &&
	       (iter->flags & BTREE_ITER_all_snapshots));

	BUG_ON(!(iter->flags & BTREE_ITER_snapshot_field) &&
	       (iter->flags & BTREE_ITER_all_snapshots) &&
	       !btree_type_has_snapshot_field(iter->btree_id));

	if (iter->update_path)
		__bch2_btree_path_verify(trans, &trans->paths[iter->update_path]);
	__bch2_btree_path_verify(trans, btree_iter_path(trans, iter));
}

static void __bch2_btree_iter_verify_entry_exit(struct btree_iter *iter)
{
	BUG_ON((iter->flags & BTREE_ITER_filter_snapshots) &&
	       !iter->pos.snapshot);

	BUG_ON(!(iter->flags & BTREE_ITER_all_snapshots) &&
	       iter->pos.snapshot != iter->snapshot);

	BUG_ON(iter->flags & BTREE_ITER_all_snapshots	? !bpos_eq(iter->pos, iter->k.p) :
	       !(iter->flags & BTREE_ITER_is_extents)	? !bkey_eq(iter->pos, iter->k.p) :
	       (bkey_lt(iter->pos, bkey_start_pos(&iter->k)) ||
		bkey_gt(iter->pos, iter->k.p)));
}

static int __bch2_btree_iter_verify_ret(struct btree_iter *iter, struct bkey_s_c k)
{
	struct btree_trans *trans = iter->trans;

	if (!(iter->flags & BTREE_ITER_filter_snapshots))
		return 0;

	if (bkey_err(k) || !k.k)
		return 0;

	BUG_ON(!bch2_snapshot_is_ancestor(trans,
					  iter->snapshot,
					  k.k->p.snapshot));

	CLASS(btree_iter, copy)(trans, iter->btree_id, iter->pos,
				BTREE_ITER_nopreserve|
				BTREE_ITER_all_snapshots);
	struct bkey_s_c prev = bkey_try(bch2_btree_iter_prev(&copy));
	if (!prev.k)
		return 0;

	if (bkey_eq(prev.k->p, k.k->p) &&
	    bch2_snapshot_is_ancestor(trans, iter->snapshot,
				      prev.k->p.snapshot) > 0) {
		struct printbuf buf1 = PRINTBUF, buf2 = PRINTBUF;

		bch2_bkey_to_text(&buf1, k.k);
		bch2_bkey_to_text(&buf2, prev.k);

		panic("iter snap %u\n"
		      "k    %s\n"
		      "prev %s\n",
		      iter->snapshot,
		      buf1.buf, buf2.buf);
	}

	return 0;
}

static inline void bch2_btree_iter_verify(struct btree_iter *iter)
{
	if (IS_ENABLED(CONFIG_BCACHEFS_DEBUG) &&
	    static_branch_unlikely(&bch2_debug_check_iterators))
		__bch2_btree_iter_verify(iter);
}

static inline void bch2_btree_iter_verify_entry_exit(struct btree_iter *iter)
{
	if (IS_ENABLED(CONFIG_BCACHEFS_DEBUG) &&
	    static_branch_unlikely(&bch2_debug_check_iterators))
		__bch2_btree_iter_verify_entry_exit(iter);
}

static inline int bch2_btree_iter_verify_ret(struct btree_iter *iter,
					     struct bkey_s_c k)
{
	return IS_ENABLED(CONFIG_BCACHEFS_DEBUG) &&
		static_branch_unlikely(&bch2_debug_check_iterators)
		? __bch2_btree_iter_verify_ret(iter, k)
		: 0;
}

void bch2_set_btree_iter_dontneed(struct btree_iter *iter)
{
	struct btree_trans *trans = iter->trans;

	if (!iter->path || trans->restarted)
		return;

	struct btree_path *path = btree_iter_path(trans, iter);
	path->preserve		= false;
	if (path->ref == 1)
		path->should_be_locked	= false;
}
/* Btree iterators: */

__must_check
int __bch2_btree_iter_traverse(struct btree_iter *iter)
{
	return bch2_btree_path_traverse(iter->trans, iter->path, iter->flags);
}

__must_check
int bch2_btree_iter_traverse(struct btree_iter *iter)
{
	struct btree_trans *trans = iter->trans;
	struct bpos search_key = btree_iter_search_key(iter);

	bch2_trans_verify_not_unlocked_or_in_restart(trans);

	iter->path = bch2_btree_path_set_pos(trans, iter->path,
					     &search_key,
					     iter->flags & BTREE_ITER_intent,
					     btree_iter_ip_allocated(iter));

	try(bch2_btree_path_traverse(iter->trans, iter->path, iter->flags));

	struct btree_path *path = btree_iter_path(trans, iter);
	if (btree_path_node(path, path->level))
		btree_path_set_should_be_locked(trans, path);
	return 0;
}

struct btree *bch2_btree_iter_peek_node(struct btree_iter *iter)
{
	struct btree_trans *trans = iter->trans;

	EBUG_ON(trans->paths[iter->path].cached);
	bch2_btree_iter_verify(iter);

	int ret = bch2_btree_iter_traverse(iter);
	struct btree *b = unlikely(ret)
		? ERR_PTR(ret)
		: path_l(btree_iter_path(trans, iter))->b;

	if (!IS_ERR_OR_NULL(b)) {
		BUG_ON(bpos_lt(b->key.k.p, iter->pos));

		bkey_init(&iter->k);
		iter->k.p = iter->pos = b->data->min_key;

		iter->path = bch2_btree_path_set_pos(trans, iter->path, &b->key.k.p,
						     iter->flags & BTREE_ITER_intent,
						     btree_iter_ip_allocated(iter));
		btree_path_set_should_be_locked(trans, btree_iter_path(trans, iter));
	}

	bch2_btree_iter_verify_entry_exit(iter);
	bch2_btree_iter_verify(iter);
	return b;
}

/* Iterate across keys (in leaf nodes only) */

inline bool bch2_btree_iter_advance(struct btree_iter *iter)
{
	struct bpos pos = iter->k.p;
	bool ret = !(iter->flags & BTREE_ITER_all_snapshots
		     ? bpos_eq(pos, SPOS_MAX)
		     : bkey_eq(pos, SPOS_MAX));

	if (ret && !(iter->flags & BTREE_ITER_is_extents))
		pos = bkey_successor(iter, pos);
	bch2_btree_iter_set_pos(iter, pos);
	return ret;
}

inline bool bch2_btree_iter_rewind(struct btree_iter *iter)
{
	struct bpos pos = iter->flags & BTREE_ITER_is_extents
		? bkey_start_pos(&iter->k)
		: iter->k.p;
	bool ret = !(iter->flags & BTREE_ITER_all_snapshots
		     ? bpos_eq(pos, POS_MIN)
		     : bkey_eq(pos, POS_MIN));

	if (ret && !(iter->flags & BTREE_ITER_is_extents))
		pos = bkey_predecessor(iter, pos);
	bch2_btree_iter_set_pos(iter, pos);
	return ret;
}

noinline
static void bch2_btree_trans_peek_prev_updates(struct btree_trans *trans, struct btree_iter *iter,
					struct bpos search_key, struct bkey_s_c *k)
{
	struct bpos end = path_l(btree_iter_path(trans, iter))->b->data->min_key;

	trans_for_each_update(trans, i)
		if (!i->key_cache_already_flushed &&
		    i->btree_id == iter->btree_id &&
		    bpos_le(i->k->k.p, search_key) &&
		    bpos_ge(i->k->k.p, k->k ? k->k->p : end)) {
			iter->k = i->k->k;
			*k = bkey_i_to_s_c(i->k);
		}
}

noinline
static void bch2_btree_trans_peek_updates(struct btree_trans *trans, struct btree_iter *iter,
				   struct bpos search_key,
				   struct bkey_s_c *k)
{
	struct btree_path *path = btree_iter_path(trans, iter);
	struct bpos end = path_l(path)->b->key.k.p;

	trans_for_each_update(trans, i)
		if (!i->key_cache_already_flushed &&
		    i->btree_id == iter->btree_id &&
		    i->level	== iter->min_depth &&
		    bpos_ge(i->k->k.p, search_key) &&
		    bpos_le(i->k->k.p, k->k ? k->k->p : end)) {
			iter->k = i->k->k;
			*k = bkey_i_to_s_c(i->k);
		}
}

noinline
static void bch2_btree_trans_peek_slot_updates(struct btree_trans *trans, struct btree_iter *iter,
					struct bkey_s_c *k)
{
	trans_for_each_update(trans, i)
		if (!i->key_cache_already_flushed &&
		    i->btree_id == iter->btree_id &&
		    i->level	== iter->min_depth &&
		    bpos_eq(i->k->k.p, iter->pos)) {
			iter->k = i->k->k;
			*k = bkey_i_to_s_c(i->k);
		}
}

static const struct bkey_i *bch2_btree_journal_peek(struct btree_trans *trans,
						    struct btree_iter *iter,
						    struct bpos search_pos,
						    struct bpos end_pos)
{
	/*
	 * BTREE_ITER_with_journal is the iterator's from its init; trans_begin()
	 * puts the transaction's ref on the journal keys once replay is done,
	 * and can't clear the flag on iterators it can't see - so the
	 * transaction's ref, not the iterator's flag, says the keys are there:
	 */
	if (!trans->journal_replay_not_finished)
		return NULL;

	struct btree_path *path = btree_iter_path(trans, iter);

	return bch2_journal_keys_peek_max(trans->c, iter->btree_id,
					   path->level,
					   search_pos,
					   end_pos,
					   &iter->journal_idx);
}

noinline
static struct bkey_s_c btree_trans_peek_slot_journal(struct btree_trans *trans,
					      struct btree_iter *iter)
{
	struct btree_path *path = btree_iter_path(trans, iter);
	const struct bkey_i *k = bch2_btree_journal_peek(trans, iter, path->pos, path->pos);

	if (k) {
		iter->k = k->k;
		return bkey_i_to_s_c(k);
	} else {
		return bkey_s_c_null;
	}
}

noinline
static void btree_trans_peek_journal(struct btree_trans *trans,
			      struct btree_iter *iter,
			      struct bpos search_key,
			      struct bkey_s_c *k)
{
	struct btree_path *path = btree_iter_path(trans, iter);
	const struct bkey_i *next_journal =
		bch2_btree_journal_peek(trans, iter, search_key,
				k->k ? k->k->p : path_l(path)->b->key.k.p);
	if (next_journal) {
		iter->k = next_journal->k;
		*k = bkey_i_to_s_c(next_journal);
	}
}

static const struct bkey_i *bch2_btree_journal_peek_prev(struct btree_trans *trans,
							 struct btree_iter *iter,
							 struct bpos search_key,
							 struct bpos end_pos)
{
	if (!trans->journal_replay_not_finished)
		return NULL;

	struct btree_path *path = btree_iter_path(trans, iter);

	return bch2_journal_keys_peek_prev_min(trans->c, iter->btree_id,
					   path->level,
					   search_key,
					   end_pos,
					   &iter->journal_idx);
}

noinline
static void btree_trans_peek_prev_journal(struct btree_trans *trans,
				   struct btree_iter *iter,
				   struct bpos search_key,
				   struct bkey_s_c *k)
{
	struct btree_path *path = btree_iter_path(trans, iter);
	const struct bkey_i *next_journal =
		bch2_btree_journal_peek_prev(trans, iter, search_key,
				k->k ? k->k->p : path_l(path)->b->data->min_key);

	if (next_journal) {
		iter->k = next_journal->k;
		*k = bkey_i_to_s_c(next_journal);
	}
}

/*
 * Checks btree key cache for key at iter->pos and returns it if present, or
 * bkey_s_c_null:
 */
noinline
static int btree_trans_peek_key_cache(struct btree_iter *iter, struct bkey_s_c *k)
{
	if (!k->k || bkey_deleted(k->k))
		return 0;

	struct btree_trans *trans = iter->trans;
	struct bch_fs *c = trans->c;

	bch2_trans_verify_not_unlocked_or_in_restart(trans);

	if ((iter->flags & BTREE_ITER_key_cache_fill) &&
	    bpos_eq(iter->pos, k->k->p))
		return 0;

	if (!bch2_btree_key_cache_find(c, iter->btree_id, k->k->p))
		return 0;

	if (!iter->key_cache_path)
		iter->key_cache_path = bch2_path_get(trans, iter->btree_id, &k->k->p,
						     !!(iter->flags & BTREE_ITER_intent), 0,
						     iter->flags|BTREE_ITER_cached|
						     BTREE_ITER_cached_nofill,
						     _THIS_IP_);

	iter->key_cache_path = bch2_btree_path_set_pos(trans, iter->key_cache_path, &k->k->p,
					iter->flags & BTREE_ITER_intent,
					btree_iter_ip_allocated(iter));

	int ret = bch2_btree_path_traverse(trans, iter->key_cache_path,
					   iter->flags|BTREE_ITER_cached) ?:
		  bch2_btree_path_relock(trans, btree_iter_path(trans, iter));
	if (unlikely(ret)) {
		bch2_btree_iter_set_pos(iter, iter->pos);
		*k = bkey_s_c_err(ret);
		return ret;
	}

	struct bkey u;
	struct bkey_s_c k2 = bch2_btree_path_peek_slot(trans->paths + iter->key_cache_path, &u);
	if (!k2.k)
		return 0;

	if ((iter->flags & BTREE_ITER_all_snapshots) &&
	    !bpos_eq(k->k->p, k2.k->p))
		return 0;

	iter->k = u;
	k->v = k2.v;
	btree_path_set_should_be_locked(trans, trans->paths + iter->key_cache_path);
	return 0;
}

noinline __cold
static void __btree_iter_peek_trace(struct bch_fs *c, struct bkey_s_c k)
{
	__event_trace(c, __btree_iter_peek, buf, ({
		int ret = bkey_err(k);
		if (ret)
			prt_str(&buf, bch2_err_str(ret));
		else if (k.k)
			bch2_bkey_val_to_text(&buf, c, k);
		else
			prt_str(&buf, "(null)");
	}));
}

static struct bkey_s_c __bch2_btree_iter_peek(struct btree_iter *iter, struct bpos *search_key,
					      const struct bpos *end)
{
	struct btree_trans *trans = iter->trans;
	struct bkey_s_c k;
	int ret;

	EBUG_ON(btree_iter_path(trans, iter)->cached);
	bch2_btree_iter_verify(iter);

	/*
	 * The last leaf a key we want can be in: an extent is in the leaf its end
	 * is in, and one starting before @end can end anywhere in @end's inode
	 */
	struct bpos last_leaf = iter->flags & BTREE_ITER_is_extents
		? POS(end->inode, KEY_OFFSET_MAX)
		: *end;

	while (1) {
		iter->path = bch2_btree_path_set_pos(trans, iter->path, search_key,
					iter->flags & BTREE_ITER_intent,
					btree_iter_ip_allocated(iter));

		ret = bch2_btree_path_traverse(trans, iter->path, iter->flags);
		if (unlikely(ret)) {
			/* ensure that iter->k is consistent with iter->pos: */
			bch2_btree_iter_set_pos(iter, iter->pos);
			k = bkey_s_c_err(ret);
			break;
		}

		struct btree_path *path = btree_iter_path(trans, iter);
		struct btree_path_level *l = path_l(path);

		if (unlikely(!l->b)) {
			/* No btree nodes at requested level: */
			bch2_btree_iter_set_pos(iter, SPOS_MAX);
			k = bkey_s_c_null;
			break;
		}

		btree_path_set_should_be_locked(trans, path);

		k = bch2_btree_path_level_peek_all(trans->c, path, &iter->k);

		if (unlikely(iter->flags & BTREE_ITER_with_key_cache) &&
		    btree_trans_peek_key_cache(iter, &k))
			break;

		/*
		 * btree_trans_peek_key_cache() does bch2_path_get() and
		 * bch2_btree_path_set_pos() on the key cache path, either of
		 * which reallocates trans->paths. l is path->l + path->level -
		 * a pointer into the path - so it dangles with it, and it's
		 * read below to pick the next search_key.
		 */
		path = btree_iter_path(trans, iter);
		l = path_l(path);

		if (unlikely(iter->flags & BTREE_ITER_with_journal))
			btree_trans_peek_journal(trans, iter, *search_key, &k);

		if (unlikely(trans->nr_updates) &&
		    !(iter->flags & BTREE_ITER_committed))
			bch2_btree_trans_peek_updates(trans, iter, *search_key, &k);

		if (likely(k.k)) {
			if (!bkey_deleted(k.k))
				break;

			/*
			 * If we've got a whiteout, and it's after the search
			 * key, advance the search key to the whiteout instead
			 * of just after the whiteout - it might be a btree
			 * whiteout, with a real key at the same position, since
			 * in the btree deleted keys sort before non deleted.
			 */
			*search_key = !bpos_eq(*search_key, k.k->p)
				? k.k->p
				: bpos_successor(k.k->p);

			/* in BTREE_ITER_filter_snapshots mode, we don't want a
			 * precise comparison against @end */
		} else if (likely(bkey_le(l->b->key.k.p, last_leaf) &&
				  bpos_lt(l->b->key.k.p, SPOS_MAX))) {
			/* Advance to next leaf node: */
			*search_key = bpos_successor(l->b->key.k.p);
		} else {
			/* End of btree: */
			bch2_btree_iter_set_pos(iter, SPOS_MAX);
			k = bkey_s_c_null;
			break;
		}
	}

	bch2_btree_iter_verify(iter);

	event_trace_fn(trans->c, __btree_iter_peek,
		       __btree_iter_peek_trace(trans->c, k));

	return k;
}

noinline __cold
static void __btree_iter_peek_max_trace(struct bch_fs *c, struct bkey_s_c k)
{
	__event_trace(c, btree_iter_peek_max, buf, ({
		int ret = bkey_err(k);
		if (ret)
			prt_str(&buf, bch2_err_str(ret));
		else if (k.k)
			bch2_bkey_val_to_text(&buf, c, k);
		else
			prt_str(&buf, "(null)");
	}));
}

enum filter_snapshots_ret {
	FILTER_SNAP_OK,		/* accept *k, exit caller's loop */
	FILTER_SNAP_CONTINUE,	/* search_key updated, retry */
	FILTER_SNAP_END,	/* out of range, terminate scan */
	FILTER_SNAP_ERR,	/* *k set to err, exit cleanly */
};

noinline
static enum filter_snapshots_ret
btree_iter_filter_snapshots(struct btree_trans *trans,
			    struct btree_iter *iter,
			    struct bkey_s_c *k,
			    struct bpos *search_key,
			    const struct bpos *end)
{
	/*
	 * We need to check against @end before FILTER_SNAPSHOTS because
	 * if we get to a different inode than requested we might be
	 * seeing keys for a different snapshot tree that will all be
	 * filtered out.
	 *
	 * But we can't do the full check here, because bkey_start_pos()
	 * isn't monotonically increasing before FILTER_SNAPSHOTS, and
	 * that's what we check against in extents mode:
	 */
	if (unlikely(!(iter->flags & BTREE_ITER_is_extents)
		     ? bkey_gt(k->k->p, *end)
		     : k->k->p.inode > end->inode))
		return FILTER_SNAP_END;

	/*
	 * If we want an update path we want to do this up front; also more
	 * efficient than letting !bch2_snapshot_is_ancestor() skip these:
	 */
	if (k->k->p.snapshot < iter->snapshot) {
		*search_key = bpos_with_snapshot(k->k->p, iter->snapshot);
		return FILTER_SNAP_CONTINUE;
	}

	if (iter->update_path &&
	    !bkey_eq(trans->paths[iter->update_path].pos, k->k->p)) {
		bch2_path_put(trans, iter->update_path,
			      iter->flags & BTREE_ITER_intent);
		iter->update_path = 0;
	}

	if ((iter->flags & BTREE_ITER_intent) &&
	    !(iter->flags & BTREE_ITER_is_extents) &&
	    !iter->update_path) {
		/*
		 * We may have to skip over keys in unrelated snapshot branches
		 * before finding the key to return; that will not be the
		 * update position for iter->snapshot - cache that:
		 */
		__btree_path_get(trans, trans->paths + iter->path, iter->flags & BTREE_ITER_intent);
		iter->update_path = iter->path;

		struct bpos with_snapshot = bpos_with_snapshot(k->k->p, iter->snapshot);
		iter->update_path = bch2_btree_path_set_pos(trans,
					iter->update_path,
					&with_snapshot,
					iter->flags & BTREE_ITER_intent,
					_THIS_IP_);
		int ret = bch2_btree_path_traverse(trans, iter->update_path, iter->flags);
		if (unlikely(ret)) {
			*k = bkey_s_c_err(ret);
			return FILTER_SNAP_ERR;
		}
	}

	/*
	 * We can never have a key in a leaf node at POS_MAX, so we don't have
	 * to check these successor() calls:
	 */
	if (!bch2_snapshot_is_ancestor(trans, iter->snapshot, k->k->p.snapshot)) {
		*search_key = bpos_successor(k->k->p);
		return FILTER_SNAP_CONTINUE;
	}

	if (!(iter->flags & BTREE_ITER_nofilter_whiteouts) &&
	    bkey_extent_whiteout(k->k)) {
		/*
		 * KEY_TYPE_extent_whiteout indicates that there are no extents
		 * overlapping with this whiteout - meaning bkey_start_pos() is
		 * monotonically increasing when including KEY_TYPE_extent_whiteout.
		 * Without this @end wouldn't be able to terminate searches and
		 * we'd have to scan through tons of whiteouts.
		 */
		if (k->k->type == KEY_TYPE_extent_whiteout &&
		    bkey_ge(k->k->p, *end))
			return FILTER_SNAP_END;

		/*
		 * Advance iter->pos incrementally, so on transaction restart we
		 * don't restart scanning over large numbers of whiteouts. For
		 * extent iterators, callers use iter->pos to mean "remaining
		 * range we are processing", so we only advance on
		 * KEY_TYPE_extent_whiteout.
		 */
		if (!(iter->flags & BTREE_ITER_is_extents) ||
		    k->k->type == KEY_TYPE_extent_whiteout)
			iter->pos = bpos_with_snapshot(k->k->p, iter->snapshot);

		*search_key = bkey_successor(iter, k->k->p);
		return FILTER_SNAP_CONTINUE;
	}

	return FILTER_SNAP_OK;
}

/*
 * BTREE_ITER_trim: an extent straddling the window [@start, @end) is returned
 * cut to it, and iter->k is cut with it: stepping by it stops at the window's
 * edge, where the walk ends. A key with no value is only its header, cut in
 * iter->k; one with a value - pointers, a reflink_p's idx, inline data, which
 * move with a cut - is cut in a copy in transaction memory. An update of what's
 * returned replaces only the window.
 */
static noinline struct bkey_s_c btree_iter_trim(struct btree_iter *iter, struct bkey_s_c k,
						struct bpos start, struct bpos end)
{
	if (bkey_le(start, bkey_start_pos(k.k)) && bkey_le(k.k->p, end))
		return k;

	if (!bkey_val_bytes(k.k)) {
		struct bkey *u = &iter->k;

		*u = *k.k;
		if (bkey_lt(bkey_start_pos(u), start))
			u->size = u->p.offset - start.offset;
		if (bkey_gt(u->p, end))
			bch2_key_resize(u, end.offset - bkey_start_offset(u));
		return (struct bkey_s_c) { u, k.v };
	}

	struct bkey_i *t = bch2_trans_kmalloc_nomemzero(iter->trans, bkey_bytes(k.k));
	if (IS_ERR(t))
		return bkey_s_c_err(PTR_ERR(t));

	bkey_reassemble(t, k);
	bch2_cut_front(iter->trans->c, start, t);
	bch2_cut_back(end, t);
	iter->k = t->k;
	return bkey_i_to_s_c(t);
}

static inline struct bkey_s_c btree_iter_maybe_trim(struct btree_iter *iter, struct bkey_s_c k,
						    struct bpos start, struct bpos end)
{
	if (likely(!bch2_btree_iter_trims(iter)) || !k.k || bkey_err(k))
		return k;

	k = btree_iter_trim(iter, k, start, end);
	/* what trimming promises: all of it in the window, and something */
	EBUG_ON(!bkey_err(k) &&
		(!k.k->size ||
		 bkey_lt(bkey_start_pos(k.k), start) ||
		 bkey_gt(k.k->p, end)));
	return k;
}

/**
 * bch2_btree_iter_peek_max() - returns first key greater than or equal to
 * iterator's current position
 * @iter:	iterator to peek from
 * @end:	search limit: returns keys less than or equal to @end
 *
 * Returns:	key if found, or an error extractable with bkey_err().
 */
struct bkey_s_c bch2_btree_iter_peek_max(struct btree_iter *iter, const struct bpos *end)
{
	struct btree_trans *trans = iter->trans;
	struct bpos search_key = btree_iter_search_key(iter);
	struct bkey_s_c k;
	int ret;

	bch2_trans_verify_not_unlocked_or_in_restart(trans);
	bch2_btree_iter_verify_entry_exit(iter);
	EBUG_ON((iter->flags & BTREE_ITER_filter_snapshots) &&
		!(iter->flags & BTREE_ITER_nofilter_whiteouts) &&
		bkey_eq(*end, POS_MAX));

	ret = trans_maybe_inject_restart(trans, _RET_IP_);
	if (unlikely(ret)) {
		k = bkey_s_c_err(ret);
		goto out_no_locked;
	}

	if (unlikely(iter->update_path)) {
		bch2_path_put(trans, iter->update_path, iter->flags & BTREE_ITER_intent);
		iter->update_path = 0;
	}

	while (1) {
		k = __bch2_btree_iter_peek(iter, &search_key, end);
		if (unlikely(!k.k))
			goto end;
		if (unlikely(bkey_err(k)))
			goto out_no_locked;

		if ((iter->flags & BTREE_ITER_filter_snapshots) &&
		    unlikely(k.k->p.snapshot != iter->snapshot ||
			     bkey_extent_whiteout(k.k) ||
			     iter->update_path)) {
			switch (btree_iter_filter_snapshots(trans, iter, &k, &search_key, end)) {
			case FILTER_SNAP_OK:
				break;
			case FILTER_SNAP_CONTINUE:
				continue;
			case FILTER_SNAP_END:
				goto end;
			case FILTER_SNAP_ERR:
				goto out_no_locked;
			}
		}

		break;
	}

	/*
	 * iter->pos should be mononotically increasing, and always be
	 * equal to the key we just returned - except extents can
	 * straddle iter->pos:
	 */
	if (!(iter->flags & BTREE_ITER_is_extents))
		iter->pos = k.k->p;
	else
		iter->pos = bkey_max(iter->pos, bkey_start_pos(k.k));

	if (!(iter->flags & BTREE_ITER_all_snapshots))
		iter->pos.snapshot = iter->snapshot;

	if (unlikely(iter->flags & BTREE_ITER_all_snapshots	? bpos_gt(iter->pos, *end) :
		     iter->flags & BTREE_ITER_is_extents	? bkey_ge(iter->pos, *end) :
								  bkey_gt(iter->pos, *end)))
		goto end;

	iter->path = bch2_btree_path_set_pos(trans, iter->path, &k.k->p,
				iter->flags & BTREE_ITER_intent,
				btree_iter_ip_allocated(iter));

	btree_path_set_should_be_locked(trans, btree_iter_path(trans, iter));
out_no_locked:
	if (iter->update_path) {
		ret = bch2_btree_path_relock(trans, trans->paths + iter->update_path);
		if (unlikely(ret))
			k = bkey_s_c_err(ret);
		else
			btree_path_set_should_be_locked(trans, trans->paths + iter->update_path);
	}

	ret = bch2_btree_iter_verify_ret(iter, k);
	if (unlikely(ret)) {
		bch2_btree_iter_set_pos(iter, iter->pos);
		k = bkey_s_c_err(ret);
	}

	bch2_btree_iter_verify_entry_exit(iter);

	k = btree_iter_maybe_trim(iter, k, iter->pos, *end);

	event_trace_fn(trans->c, btree_iter_peek_max,
		       __btree_iter_peek_max_trace(trans->c, k));

	return k;
end:
	bch2_btree_iter_set_pos(iter, *end);
	k = bkey_s_c_null;
	goto out_no_locked;
}

/**
 * bch2_btree_iter_next() - returns first key greater than iterator's current
 * position
 * @iter:	iterator to peek from
 *
 * Returns:	key if found, or an error extractable with bkey_err().
 */
struct bkey_s_c bch2_btree_iter_next(struct btree_iter *iter)
{
	if (!bch2_btree_iter_advance(iter))
		return bkey_s_c_null;

	return bch2_btree_iter_peek(iter);
}

static struct bkey_s_c __bch2_btree_iter_peek_prev(struct btree_iter *iter, struct bpos search_key)
{
	struct btree_trans *trans = iter->trans;
	struct bkey_s_c k;

	bch2_btree_iter_verify(iter);

	while (1) {
		iter->path = bch2_btree_path_set_pos(trans, iter->path, &search_key,
					iter->flags & BTREE_ITER_intent,
					btree_iter_ip_allocated(iter));

		int ret = bch2_btree_path_traverse(trans, iter->path, iter->flags);
		if (unlikely(ret)) {
			/* ensure that iter->k is consistent with iter->pos: */
			bch2_btree_iter_set_pos(iter, iter->pos);
			k = bkey_s_c_err(ret);
			break;
		}

		struct btree_path *path = btree_iter_path(trans, iter);
		struct btree_path_level *l = path_l(path);

		if (unlikely(!l->b)) {
			/* No btree nodes at requested level: */
			bch2_btree_iter_set_pos(iter, SPOS_MAX);
			k = bkey_s_c_null;
			break;
		}
		iter->path = bch2_btree_path_make_mut(trans, iter->path,
				iter->flags & BTREE_ITER_intent,
				btree_iter_ip_allocated(iter));
		path = btree_iter_path(trans, iter);
		l = path_l(path);

		btree_path_set_should_be_locked(trans, path);

		k = bch2_btree_path_level_peek_all(trans->c, path, &iter->k);

		if (!k.k || bpos_gt(k.k->p, search_key)) {
			k = bch2_btree_path_level_prev(trans, path, &iter->k);

			EBUG_ON(k.k && bpos_gt(k.k->p, search_key));
		}

		if (unlikely(iter->flags & BTREE_ITER_with_key_cache) &&
		    btree_trans_peek_key_cache(iter, &k))
			break;

		/* See __bch2_btree_iter_peek(): peek_key_cache() reallocates
		 * trans->paths, and l points into the path. */
		path = btree_iter_path(trans, iter);
		l = path_l(path);

		if (unlikely(iter->flags & BTREE_ITER_with_journal))
			btree_trans_peek_prev_journal(trans, iter, search_key, &k);

		if (unlikely(trans->nr_updates) &&
		    !(iter->flags & BTREE_ITER_committed))
			bch2_btree_trans_peek_prev_updates(trans, iter, search_key, &k);

		if (likely(k.k && !bkey_deleted(k.k))) {
			break;
		} else if (k.k) {
			search_key = bpos_predecessor(k.k->p);
		} else if (likely(!bpos_eq(l->b->data->min_key, POS_MIN))) {
			/* Advance to previous leaf node: */
			search_key = bpos_predecessor(l->b->data->min_key);
		} else {
			/* Start of btree: */
			bch2_btree_iter_set_pos(iter, POS_MIN);
			k = bkey_s_c_null;
			break;
		}
	}

	/*
	 * TODO: unlike bch2_btree_iter_peek_max(), which does a normalizing
	 * bch2_btree_path_set_pos(&k.k->p) on return, peek_prev sets path->pos
	 * directly (bch2_btree_path_level_prev / the k.k->p store above) and never
	 * re-runs up_until_good_node - so on return path->pos may be
	 * inconsistent with path->l[level].b above the leaf. The only guard is
	 * bch2_btree_iter_verify (static-branch gated, runs rarely in CI), so
	 * the inconsistency escapes to the always-on
	 * bch2_trans_node_verify_not_in_iters() BUG in a later node split
	 * (seen: copygc_torture_no_checksum, inject_transaction_restarts=1).
	 * Fix likely mirrors peek_max's trailing set_pos, but the reverse-
	 * iteration pos semantics need thought first.
	 */
	bch2_btree_iter_verify(iter);
	return k;
}

noinline __cold
static void __btree_iter_peek_prev_min_trace(struct bch_fs *c, struct bkey_s_c k)
{
	__event_trace(c, btree_iter_peek_prev_min, buf, ({
		int ret = bkey_err(k);
		if (ret)
			prt_str(&buf, bch2_err_str(ret));
		else if (k.k)
			bch2_bkey_val_to_text(&buf, c, k);
		else
			prt_str(&buf, "(null)");
	}));
}

/*
 * The key visible in @iter->snapshot at exactly @pos - for an extents
 * iterator, the extent ending at @pos - or bkey_s_c_null if nothing is: the
 * nearest version among iter->snapshot and its ancestors, unless that's a
 * whiteout.
 *
 * Resolved forwards, as peek_slot() does: our own snapshot's version comes
 * first, then each ancestor's in turn, so the first one visible is the
 * answer, journal keys and the transaction's updates included. Searching
 * backwards meets the furthest ancestor first and would have to remember
 * candidates.
 *
 * Done on a copy without is_extents, so the lookup is of exactly @pos: an
 * extents peek_slot() of the extent containing @pos - 1 would scan on to the
 * next visible extent, and a backward search calling this at each position
 * through a run of whiteouts would rescan it from each.
 *
 * On success iter->path is the copy's, at the key, and iter->k holds it.
 */
static struct bkey_s_c btree_iter_peek_visible_at(struct btree_iter *iter, struct bpos pos)
{
	CLASS(btree_iter_copy, iter2)(iter);
	iter2.flags &= ~(BTREE_ITER_is_extents|BTREE_ITER_nofilter_whiteouts);

	bch2_btree_iter_set_pos(&iter2, SPOS(pos.inode, pos.offset, iter->snapshot));

	struct bkey_s_c k = bch2_btree_iter_peek_slot(&iter2);
	if (bkey_err(k))
		return k;
	/* A hole, or a whiteout - and it points into iter2: */
	if (!k.k || bkey_deleted(k.k))
		return bkey_s_c_null;

	swap(iter->path,		iter2.path);
	swap(iter->key_cache_path,	iter2.key_cache_path);
	iter->k = iter2.k;
	k.k = &iter->k;
	return k;
}

static struct bkey_s_c __bch2_btree_iter_peek_slot(struct btree_iter *, struct bpos,
						   unsigned long);

/**
 * bch2_btree_iter_peek_prev_min() - returns first key less than or equal to
 * iterator's current position
 * @iter:	iterator to peek from
 * @end:	search limit: returns keys greater than or equal to @end
 *
 * With BTREE_ITER_filter_snapshots: the greatest key visible in
 * iter->snapshot at or below iter->pos - for extents, starting before it -
 * and at or above @end. What's visible at a position is the version there of
 * the nearest of iter->snapshot and its ancestors, unless that's a whiteout:
 * then nothing is.
 *
 * Snapshot IDs decrease from the root down, so searching backwards meets a
 * position's versions furthest ancestor first; the nearest is found forwards
 * instead (btree_iter_peek_visible_at()), once per position that has one.
 *
 * With BTREE_ITER_trim, an extent is cut to [@end, iter->pos), and iter->pos
 * is left at the end of what's returned.
 *
 * Returns:	key if found, or an error extractable with bkey_err().
 */
struct bkey_s_c bch2_btree_iter_peek_prev_min(struct btree_iter *iter, struct bpos end)
{
	/* trimming, the window is [@end, iter->pos): nothing in an empty one */
	if (bch2_btree_iter_trims(iter) && bkey_le(iter->pos, end)) {
		bch2_btree_iter_set_pos(iter, end);
		return bkey_s_c_null;
	}

	if ((iter->flags & (BTREE_ITER_is_extents|BTREE_ITER_filter_snapshots)) &&
	   !bkey_eq(iter->pos, POS_MAX) &&
	   !((iter->flags & BTREE_ITER_is_extents) &&
	     iter->pos.offset == U64_MAX)) {

		/*
		 * bkey_start_pos(), for extents, is not monotonically
		 * increasing until after filtering for snapshots:
		 *
		 * Thus, for extents we need to search forward until we find a
		 * real visible extents - easiest to just use peek_slot() (which
		 * internally uses peek() for extents) - untrimmed, as that would
		 * cut it to the wrong side of iter->pos
		 */
		struct bkey_s_c k = __bch2_btree_iter_peek_slot(iter, SPOS_MAX, _RET_IP_);
		if (bkey_err(k))
			return k;

		if (!bkey_deleted(k.k) &&
		    (!(iter->flags & BTREE_ITER_is_extents) ||
		     bkey_lt(bkey_start_pos(k.k), iter->pos)))
			return btree_iter_maybe_trim(iter, k, end, iter->pos);
	}

	struct btree_trans *trans = iter->trans;
	struct bpos search_key = iter->pos;
	struct bkey_s_c k;

	/*
	 * Every snapshot's key at this position: an ancestor's sorts after
	 * iter->snapshot's, and searching back from iter->pos would skip it -
	 * for extents, the one ending here:
	 */
	if (iter->flags & BTREE_ITER_filter_snapshots)
		search_key.snapshot = U32_MAX;

	bch2_trans_verify_not_unlocked_or_in_restart(trans);
	bch2_btree_iter_verify_entry_exit(iter);
	EBUG_ON((iter->flags & BTREE_ITER_filter_snapshots) && iter->pos.inode != end.inode);

	int ret = trans_maybe_inject_restart(trans, _RET_IP_);
	if (unlikely(ret)) {
		k = bkey_s_c_err(ret);
		goto out_no_locked;
	}

	while (1) {
		k = __bch2_btree_iter_peek_prev(iter, search_key);
		if (unlikely(!k.k))
			goto end;
		if (unlikely(bkey_err(k)))
			goto out_no_locked;

		if (iter->flags & BTREE_ITER_filter_snapshots) {
			/*
			 * We need to check against @end before FILTER_SNAPSHOTS because
			 * if we get to a different inode that requested we might be
			 * seeing keys for a different snapshot tree that will all be
			 * filtered out.
			 */
			if (unlikely(bkey_lt(k.k->p, end)))
				goto end;

			if (!bch2_snapshot_is_ancestor(trans, iter->snapshot, k.k->p.snapshot)) {
				search_key = bpos_predecessor(k.k->p);
				continue;
			}

			/*
			 * A version visible in iter->snapshot is at this
			 * position: which one we see - if any - is decided
			 * forwards, from iter->snapshot's own up:
			 */
			struct bpos pos = k.k->p;

			k = btree_iter_peek_visible_at(iter, pos);
			if (unlikely(bkey_err(k))) {
				/*
				 * iter->k still holds the version
				 * __bch2_btree_iter_peek_prev() found: make it
				 * consistent with iter->pos, as its own error
				 * path does:
				 */
				bch2_btree_iter_set_pos(iter, iter->pos);
				goto out_no_locked;
			}

			if (!k.k) {
				if (!bkey_gt(pos, end))
					goto end;

				search_key = bkey_predecessor(iter, pos);
				search_key.snapshot = U32_MAX;
				continue;
			}
		}

		EBUG_ON(iter->flags & BTREE_ITER_all_snapshots		? bpos_gt(k.k->p, iter->pos) :
			iter->flags & BTREE_ITER_is_extents		? bkey_ge(bkey_start_pos(k.k), iter->pos) :
									  bkey_gt(k.k->p, iter->pos));

		if (unlikely(iter->flags & BTREE_ITER_all_snapshots	? bpos_lt(k.k->p, end) :
			     iter->flags & BTREE_ITER_is_extents	? bkey_le(k.k->p, end) :
									  bkey_lt(k.k->p, end)))
			goto end;

		break;
	}

	/* Extents can straddle iter->pos: */
	iter->pos = bpos_min(iter->pos, k.k->p);

	if (iter->flags & BTREE_ITER_filter_snapshots)
		iter->pos.snapshot = iter->snapshot;

	k = btree_iter_maybe_trim(iter, k, end, iter->pos);
out_no_locked:
	bch2_btree_iter_verify_entry_exit(iter);
	bch2_btree_iter_verify(iter);

	event_trace_fn(trans->c, btree_iter_peek_prev_min,
		       __btree_iter_peek_prev_min_trace(trans->c, k));

	return k;
end:
	bch2_btree_iter_set_pos(iter, end);
	k = bkey_s_c_null;
	goto out_no_locked;
}

/**
 * bch2_btree_iter_prev() - returns first key less than iterator's current
 * position
 * @iter:	iterator to peek from
 *
 * Returns:	key if found, or an error extractable with bkey_err().
 */
struct bkey_s_c bch2_btree_iter_prev(struct btree_iter *iter)
{
	if (!bch2_btree_iter_rewind(iter))
		return bkey_s_c_null;

	return bch2_btree_iter_peek_prev(iter);
}

noinline __cold
static void __btree_iter_peek_slot_trace(struct bch_fs *c, struct bkey_s_c k)
{
	__event_trace(c, btree_iter_peek_slot, buf, ({
		int ret = bkey_err(k);
		if (ret)
			prt_str(&buf, bch2_err_str(ret));
		else if (k.k)
			bch2_bkey_val_to_text(&buf, c, k);
		else
			prt_str(&buf, "(null)");
	}));
}

static struct bkey_s_c __bch2_btree_iter_peek_slot(struct btree_iter *iter, struct bpos end,
						   unsigned long ip)
{
	struct btree_trans *trans = iter->trans;
	struct bpos search_key;
	struct bkey_s_c k;
	int ret;

	bch2_trans_verify_not_unlocked_or_in_restart(trans);
	bch2_btree_iter_verify(iter);
	bch2_btree_iter_verify_entry_exit(iter);
	EBUG_ON(btree_iter_path(trans, iter)->level && (iter->flags & BTREE_ITER_with_key_cache));

	ret = trans_maybe_inject_restart(trans, ip);
	if (unlikely(ret)) {
		k = bkey_s_c_err(ret);
		goto out;
	}

	/* extents can't span inode numbers: */
	if ((iter->flags & BTREE_ITER_is_extents) &&
	    unlikely(iter->pos.offset == KEY_OFFSET_MAX)) {
		if (iter->pos.inode == KEY_INODE_MAX) {
			k = bkey_s_c_null;
			goto out2;
		}

		bch2_btree_iter_set_pos(iter, bpos_nosnap_successor(iter->pos));
	}

	search_key = btree_iter_search_key(iter);
	iter->path = bch2_btree_path_set_pos(trans, iter->path, &search_key,
					iter->flags & BTREE_ITER_intent,
					btree_iter_ip_allocated(iter));

	ret = bch2_btree_path_traverse(trans, iter->path, iter->flags);
	if (unlikely(ret)) {
		k = bkey_s_c_err(ret);
		goto out;
	}

	struct btree_path *path = btree_iter_path(trans, iter);
	if (unlikely(!btree_path_node(path, path->level))) {
		k = bkey_s_c_null;
		goto out2;
	}

	btree_path_set_should_be_locked(trans, path);

	if ((iter->flags & BTREE_ITER_cached) ||
	    !(iter->flags & (BTREE_ITER_is_extents|BTREE_ITER_filter_snapshots))) {
		/*
		 * Consult sources in the same order as bch2_btree_iter_peek_max():
		 * btree, then key cache, then journal, then the transaction's own
		 * updates - each overlaying the last, so an in-transaction update
		 * wins. Crucially the key cache is peeked before trans updates, so
		 * the key_cache_path is established even when we already have an
		 * update for this key; otherwise a second update to the same cached
		 * key in one transaction would skip the cache and livelock against
		 * a dirty cached entry in bch2_trans_update_get_key_cache().
		 */
		k = bch2_btree_path_peek_slot(btree_iter_path(trans, iter), &iter->k);

		if (unlikely(iter->flags & BTREE_ITER_with_key_cache) &&
		    btree_trans_peek_key_cache(iter, &k))
			goto out;

		if (unlikely(iter->flags & BTREE_ITER_with_journal)) {
			struct bkey_s_c j = btree_trans_peek_slot_journal(trans, iter);
			if (j.k)
				k = j;
		}

		if (unlikely(trans->nr_updates) &&
		    !(iter->flags & BTREE_ITER_committed))
			bch2_btree_trans_peek_slot_updates(trans, iter, &k);

		if (unlikely(!k.k))
			goto out;

		if (unlikely(bkey_extent_whiteout(k.k) &&
			     (iter->flags & BTREE_ITER_filter_snapshots) &&
			     !(iter->flags & BTREE_ITER_nofilter_whiteouts)))
			iter->k.type = KEY_TYPE_deleted;
	} else {
		struct bpos next;
		struct bpos search_end = iter->pos;

		/*
		 * To the inode's end, not @end: an extent is in the leaf its end is
		 * in, and one starting before @end can end past the leaf a peek to
		 * @end stops in
		 */
		if (iter->flags & BTREE_ITER_is_extents)
			search_end.offset = U64_MAX;

		EBUG_ON(btree_iter_path(trans, iter)->level);

		CLASS(btree_iter_copy, iter2)(iter);
		iter2.flags |= BTREE_ITER_nofilter_whiteouts;
		/* k below is iter2's value under iter->k, a copy of iter2.k: untrimmed, they agree */
		iter2.flags &= ~BTREE_ITER_trim;

		while (1) {
			k = bch2_btree_iter_peek_max(&iter2, &search_end);
			if ((iter2.flags & BTREE_ITER_is_extents) &&
			    k.k &&
			    !bkey_err(k) &&
			    k.k->type == KEY_TYPE_whiteout) {
				bch2_btree_iter_set_pos(&iter2, k.k->p);
				continue;
			}

			break;
		}

		if (k.k && !bkey_err(k)) {
			swap(iter->key_cache_path, iter2.key_cache_path);
			iter->k = iter2.k;
			k.k = &iter->k;
		}

		if (unlikely(bkey_err(k)))
			goto out;

		if (unlikely(k.k &&
			     bkey_extent_whiteout(k.k) &&
			     (iter->flags & BTREE_ITER_filter_snapshots) &&
			     !(iter->flags & BTREE_ITER_nofilter_whiteouts)))
			iter->k.type = KEY_TYPE_deleted;

		next = k.k ? bkey_start_pos(k.k) : POS_MAX;
		/* a hole ends at @end at the latest */
		if ((iter->flags & BTREE_ITER_is_extents) && bkey_lt(end, next))
			next = end;

		if (bkey_lt(iter->pos, next)) {
			bkey_init(&iter->k);
			iter->k.p = iter->pos;

			if (iter->flags & BTREE_ITER_is_extents) {
				bch2_key_resize(&iter->k,
						min_t(u64, KEY_SIZE_MAX,
						      (next.inode == iter->pos.inode
						       ? next.offset
						       : KEY_OFFSET_MAX) -
						      iter->pos.offset));
				EBUG_ON(!iter->k.size);
			}

			k = (struct bkey_s_c) { &iter->k, NULL };
		}
	}
out:
	bch2_btree_iter_verify_entry_exit(iter);
	bch2_btree_iter_verify(iter);
	ret = bch2_btree_iter_verify_ret(iter, k);
	if (unlikely(ret))
		k = bkey_s_c_err(ret);
out2:
	event_trace_fn(trans->c, btree_iter_peek_slot,
		       __btree_iter_peek_slot_trace(trans->c, k));

	return k;
}

/**
 * bch2_btree_iter_peek_slot() - the key at the iterator's position, or a hole
 * there - a deleted key - if there's none
 * @iter:	iterator to peek from
 *
 * Returns:	key if found, or an error extractable with bkey_err().
 */
struct bkey_s_c bch2_btree_iter_peek_slot(struct btree_iter *iter)
{
	struct bkey_s_c k = __bch2_btree_iter_peek_slot(iter, SPOS_MAX, _RET_IP_);

	return btree_iter_maybe_trim(iter, k, iter->pos, SPOS_MAX);
}

/**
 * bch2_btree_iter_peek_slot_max() - bch2_btree_iter_peek_slot() with an end:
 * for extents, a hole ends at @end at the latest, and with BTREE_ITER_trim the
 * key is cut to [iter->pos, @end)
 * @iter:	iterator to peek from
 * @end:	the end of the window, past the iterator's position
 *
 * Returns:	key if found, or an error extractable with bkey_err().
 */
struct bkey_s_c bch2_btree_iter_peek_slot_max(struct btree_iter *iter, struct bpos end)
{
	EBUG_ON((iter->flags & BTREE_ITER_is_extents) && !bkey_lt(iter->pos, end));

	struct bkey_s_c k = __bch2_btree_iter_peek_slot(iter, end, _RET_IP_);

	return btree_iter_maybe_trim(iter, k, iter->pos, end);
}

struct bkey_s_c bch2_btree_iter_next_slot(struct btree_iter *iter)
{
	if (!bch2_btree_iter_advance(iter))
		return bkey_s_c_null;

	return bch2_btree_iter_peek_slot(iter);
}

struct bkey_s_c bch2_btree_iter_prev_slot(struct btree_iter *iter)
{
	/* reverse slots aren't defined for extents: peek_slot() is the slot from @pos on */
	BUG_ON(iter->flags & BTREE_ITER_is_extents);

	if (!bch2_btree_iter_rewind(iter))
		return bkey_s_c_null;

	return bch2_btree_iter_peek_slot(iter);
}

/* Obsolete, but still used by rust wrapper in -tools */
struct bkey_s_c bch2_btree_iter_peek_and_restart_outlined(struct btree_iter *iter)
{
	struct bkey_s_c k;

	lockrestart_do(iter->trans, bkey_err(k = bch2_btree_iter_peek_type(iter, iter->flags)));
	return k;
}

struct bkey_s_c bch2_btree_iter_peek_root(struct btree_trans *trans, struct btree_iter *iter,
					  enum btree_id btree, unsigned level)
{
	struct bch_fs *c = trans->c;

	while (level == bch2_btree_id_root_b(c, btree)->c.level + 1) {
		bch2_trans_node_iter_init(trans, iter, btree, POS_MIN, 0, level - 1,
					  BTREE_ITER_not_extents|
					  BTREE_ITER_all_snapshots);
		struct btree *b = bch2_btree_iter_peek_node(iter);
		int ret = PTR_ERR_OR_ZERO(b);
		if (ret)
			return bkey_s_c_err(ret);

		if (b != btree_node_root(c, b))
			continue;

		if (btree_node_fake(b))
			break;

		return bkey_i_to_s_c(&b->key);
	}

	return bkey_s_c_null;
}

void bch2_trans_iter_exit(struct btree_iter *iter)
{
	struct btree_trans *trans = iter->trans;

	if (iter->update_path)
		bch2_path_put(trans, iter->update_path,
			      iter->flags & BTREE_ITER_intent);
	if (iter->path)
		bch2_path_put(trans, iter->path,
			      iter->flags & BTREE_ITER_intent);
	if (iter->key_cache_path)
		bch2_path_put(trans, iter->key_cache_path,
			      iter->flags & BTREE_ITER_intent);
	iter->path		= 0;
	iter->update_path	= 0;
	iter->key_cache_path	= 0;
	iter->trans		= NULL;
}

void bch2_trans_iter_init_outlined(struct btree_trans *trans,
			  struct btree_iter *iter,
			  enum btree_id btree_id, struct bpos pos,
			  enum btree_iter_update_trigger_flags flags,
			  unsigned long ip)
{
	bch2_trans_iter_init_common(trans, iter, btree_id, pos, 0, 0,
			       bch2_btree_iter_flags(trans, btree_id, 0, flags),
			       ip);
}

/*
 * Check iterator parameters from an untrusted source (e.g. an ioctl) against
 * the preconditions the iterator code enforces with assertions. Kernel
 * callers know these rules; user-controlled (btree, level, range, flags)
 * combinations must be validated first:
 */
bool bch2_btree_iter_params_valid(enum btree_id btree, unsigned level,
				  struct bpos start, struct bpos end,
				  enum btree_iter_update_trigger_flags flags)
{
	if (btree >= BTREE_ID_NR)
		return false;

	if (level >= BTREE_MAX_DEPTH)
		return false;

	/* Interior nodes span snapshots; filtering only makes sense at level 0: */
	if (level && !(flags & BTREE_ITER_all_snapshots))
		return false;

	if (btree_type_has_snapshots(btree) &&
	    !(flags & BTREE_ITER_all_snapshots)) {
		/* No snapshot to filter against: */
		if (!start.snapshot)
			return false;

		/* Whiteout filtering peeks ahead of the end pos: */
		if (bpos_eq(end, POS_MAX))
			return false;

		/* Backwards filtered iteration requires a single inode: */
		if ((flags & BTREE_ITER_prev) &&
		    start.inode != end.inode)
			return false;
	}

	/* Reverse slots aren't defined for extents: */
	if ((flags & BTREE_ITER_prev) &&
	    (flags & BTREE_ITER_slots) &&
	    !(flags & (BTREE_ITER_all_snapshots|BTREE_ITER_not_extents)) &&
	    btree_id_is_extents(btree))
		return false;

	return true;
}

void __bch2_trans_iter_init_ll(struct btree_trans *trans,
			       struct btree_iter *iter,
			       enum btree_id btree_id,
			       struct bpos pos,
			       unsigned locks_want,
			       unsigned depth,
			       enum btree_iter_update_trigger_flags flags,
			       unsigned long ip)
{
	bch2_trans_iter_init_common(trans, iter, btree_id, pos, locks_want, depth,
			       bch2_btree_iter_flags(trans, btree_id, depth, flags),
			       ip);

	iter->min_depth	= depth;
#ifdef CONFIG_BCACHEFS_DEBUG
	struct btree_path *path = btree_iter_path(trans, iter);
	BUG_ON(path->locks_want	 < min(locks_want, BTREE_MAX_DEPTH));
	BUG_ON(path->level	!= depth);
	BUG_ON(iter->min_depth	!= depth);
#endif
}

void __bch2_trans_node_iter_init(struct btree_trans *trans,
				 struct btree_iter *iter,
				 enum btree_id btree_id,
				 struct bpos pos,
				 unsigned locks_want,
				 unsigned depth,
				 enum btree_iter_update_trigger_flags flags)
{
	flags |= BTREE_ITER_not_extents;
	flags |= BTREE_ITER_snapshot_field;
	flags |= BTREE_ITER_all_snapshots;

	if (!depth && btree_id_cached(btree_id))
		flags |= BTREE_ITER_with_key_cache;

	__bch2_trans_iter_init_ll(trans, iter, btree_id, pos, locks_want, depth,
				  flags, _RET_IP_);
}

void bch2_trans_copy_iter(struct btree_iter *dst, struct btree_iter *src)
{
	struct btree_trans *trans = src->trans;

	*dst = *src;
#ifdef TRACK_PATH_ALLOCATED
	dst->ip_allocated = _RET_IP_;
#endif
	if (src->path)
		__btree_path_get(trans, trans->paths + src->path, src->flags & BTREE_ITER_intent);
	if (src->update_path)
		__btree_path_get(trans, trans->paths + src->update_path, src->flags & BTREE_ITER_intent);
	dst->key_cache_path = 0;
}

void bch2_fs_btree_iter_exit(struct bch_fs *c)
{
	struct btree_transaction_stats *s;
	struct btree_trans *trans;
	int cpu;

	if (c->btree.trans.bufs)
		for_each_possible_cpu(cpu) {
			struct btree_trans *trans =
				per_cpu_ptr(c->btree.trans.bufs, cpu)->trans;

			if (trans) {
				seqmutex_lock(&c->btree.trans.lock);
				list_del(&trans->list);
				seqmutex_unlock(&c->btree.trans.lock);
			}
			kfree(trans);
		}
	free_percpu(c->btree.trans.bufs);

	trans = list_first_entry_or_null(&c->btree.trans.list, struct btree_trans, list);
	if (trans)
		panic("%s leaked btree_trans\n", trans->fn);

	for (s = c->btree.trans.stats;
	     s < c->btree.trans.stats + ARRAY_SIZE(c->btree.trans.stats);
	     s++) {
#ifdef CONFIG_BCACHEFS_TRANS_KMALLOC_TRACE
		darray_exit(&s->trans_kmalloc_trace);
#endif
		kfree(s->max_paths_text);
	}

	printbuf_exit(&c->btree.trans.stats_json_buf);

	/* Drain pending trans_rcu_free callbacks before the pool is gone. */
	rcu_barrier();
}

void bch2_fs_btree_iter_init_early(struct bch_fs *c)
{
	INIT_LIST_HEAD(&c->btree.trans.list);
	seqmutex_init(&c->btree.trans.lock);
	mutex_init(&c->btree.trans.stats_json_lock);
	c->btree.trans.stats_json_buf = PRINTBUF;
}

int bch2_fs_btree_iter_init(struct bch_fs *c)
{
	c->btree.trans.bufs = alloc_percpu(struct btree_trans_buf);
	if (!c->btree.trans.bufs)
		return -ENOMEM;

	/*
	 * static annotation (hackily done) for lock ordering of reclaim vs.
	 * btree node locks:
	 */
#ifdef CONFIG_LOCKDEP
	fs_reclaim_acquire(GFP_KERNEL);
	struct btree_trans *trans = bch2_trans_get(c);
	bch2_trans_set_locked(trans, false);
	bch2_trans_put(trans);
	fs_reclaim_release(GFP_KERNEL);
#endif

	return 0;

}
