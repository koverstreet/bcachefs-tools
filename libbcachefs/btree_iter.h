/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_BTREE_ITER_H
#define _BCACHEFS_BTREE_ITER_H

#include "bset.h"
#include "btree_types.h"
#include "trace.h"

void bch2_trans_updates_to_text(struct printbuf *, struct btree_trans *);
void bch2_btree_path_to_text(struct printbuf *, struct btree_trans *, btree_path_idx_t);
void bch2_trans_paths_to_text(struct printbuf *, struct btree_trans *);
void bch2_dump_trans_paths_updates(struct btree_trans *);

static inline int __bkey_err(const struct bkey *k)
{
	return PTR_ERR_OR_ZERO(k);
}

#define bkey_err(_k)	__bkey_err((_k).k)

static inline void __btree_path_get(struct btree_trans *trans, struct btree_path *path, bool intent)
{
	unsigned idx = path - trans->paths;

	EBUG_ON(idx >= trans->nr_paths);
	EBUG_ON(!test_bit(idx, trans->paths_allocated));
	if (unlikely(path->ref == U8_MAX)) {
		bch2_dump_trans_paths_updates(trans);
		panic("path %u refcount overflow\n", idx);
	}

	path->ref++;
	path->intent_ref += intent;
	trace_btree_path_get_ll(trans, path);
}

static inline bool __btree_path_put(struct btree_trans *trans, struct btree_path *path, bool intent)
{
	EBUG_ON(path - trans->paths >= trans->nr_paths);
	EBUG_ON(!test_bit(path - trans->paths, trans->paths_allocated));
	EBUG_ON(!path->ref);
	EBUG_ON(!path->intent_ref && intent);

	trace_btree_path_put_ll(trans, path);
	path->intent_ref -= intent;
	return --path->ref == 0;
}

static inline void btree_path_set_dirty(struct btree_trans *trans,
					struct btree_path *path,
					enum btree_path_uptodate u)
{
	BUG_ON(path->should_be_locked && trans->locked && !trans->restarted);
	path->uptodate = max_t(unsigned, path->uptodate, u);
}

static inline struct btree *btree_path_node(struct btree_path *path,
					    unsigned level)
{
	return level < BTREE_MAX_DEPTH ? path->l[level].b : NULL;
}

static inline bool btree_node_lock_seq_matches(const struct btree_path *path,
					const struct btree *b, unsigned level)
{
	return path->l[level].lock_seq == six_lock_seq(&b->c.lock);
}

static inline struct btree *btree_node_parent(struct btree_path *path,
					      struct btree *b)
{
	return btree_path_node(path, b->c.level + 1);
}

/* Iterate over paths within a transaction: */

void __bch2_btree_trans_sort_paths(struct btree_trans *);

static inline void btree_trans_sort_paths(struct btree_trans *trans)
{
	if (!IS_ENABLED(CONFIG_BCACHEFS_DEBUG) &&
	    trans->paths_sorted)
		return;
	__bch2_btree_trans_sort_paths(trans);
}

static inline unsigned long *trans_paths_nr(struct btree_path *paths)
{
	return &container_of(paths, struct btree_trans_paths, paths[0])->nr_paths;
}

static inline unsigned long *trans_paths_allocated(struct btree_path *paths)
{
	unsigned long *v = trans_paths_nr(paths);
	return v - BITS_TO_LONGS(*v);
}

#define trans_for_each_path_idx_from(_paths_allocated, _nr, _idx, _start)\
	for (_idx = _start;						\
	     (_idx = find_next_bit(_paths_allocated, _nr, _idx)) < _nr;	\
	     _idx++)

static inline struct btree_path *
__trans_next_path(struct btree_trans *trans, unsigned *idx)
{
	unsigned long *w = trans->paths_allocated + *idx / BITS_PER_LONG;
	/*
	 * Open coded find_next_bit(), because
	 *  - this is fast path, we can't afford the function call
	 *  - and we know that nr_paths is a multiple of BITS_PER_LONG,
	 */
	while (*idx < trans->nr_paths) {
		unsigned long v = *w >> (*idx & (BITS_PER_LONG - 1));
		if (v) {
			*idx += __ffs(v);
			return trans->paths + *idx;
		}

		*idx += BITS_PER_LONG;
		*idx &= ~(BITS_PER_LONG - 1);
		w++;
	}

	return NULL;
}

/*
 * This version is intended to be safe for use on a btree_trans that is owned by
 * another thread, for bch2_btree_trans_to_text();
 */
#define trans_for_each_path_from(_trans, _path, _idx, _start)		\
	for (_idx = _start;						\
	     (_path = __trans_next_path((_trans), &_idx));		\
	     _idx++)

#define trans_for_each_path(_trans, _path, _idx)			\
	trans_for_each_path_from(_trans, _path, _idx, 1)

static inline struct btree_path *next_btree_path(struct btree_trans *trans, struct btree_path *path)
{
	unsigned idx = path ? path->sorted_idx + 1 : 0;

	EBUG_ON(idx > trans->nr_sorted);

	return idx < trans->nr_sorted
		? trans->paths + trans->sorted[idx]
		: NULL;
}

static inline struct btree_path *prev_btree_path(struct btree_trans *trans, struct btree_path *path)
{
	unsigned idx = path ? path->sorted_idx : trans->nr_sorted;

	return idx
		? trans->paths + trans->sorted[idx - 1]
		: NULL;
}

#define trans_for_each_path_idx_inorder(_trans, _iter)			\
	for (_iter = (struct trans_for_each_path_inorder_iter) { 0 };	\
	     (_iter.path_idx = trans->sorted[_iter.sorted_idx],		\
	      _iter.sorted_idx < (_trans)->nr_sorted);			\
	     _iter.sorted_idx++)

struct trans_for_each_path_inorder_iter {
	btree_path_idx_t	sorted_idx;
	btree_path_idx_t	path_idx;
};

#define trans_for_each_path_inorder(_trans, _path, _iter)		\
	for (_iter = (struct trans_for_each_path_inorder_iter) { 0 };	\
	     (_iter.path_idx = trans->sorted[_iter.sorted_idx],		\
	      _path = (_trans)->paths + _iter.path_idx,			\
	      _iter.sorted_idx < (_trans)->nr_sorted);			\
	     _iter.sorted_idx++)

#define trans_for_each_path_inorder_reverse(_trans, _path, _i)		\
	for (_i = trans->nr_sorted - 1;					\
	     ((_path) = (_trans)->paths + trans->sorted[_i]), (_i) >= 0;\
	     --_i)

static inline bool __path_has_node(const struct btree_path *path,
				   const struct btree *b)
{
	return path->l[b->c.level].b == b &&
		btree_node_lock_seq_matches(path, b, b->c.level);
}

static inline struct btree_path *
__trans_next_path_with_node(struct btree_trans *trans, struct btree *b,
			    unsigned *idx)
{
	struct btree_path *path;

	while ((path = __trans_next_path(trans, idx)) &&
		!__path_has_node(path, b))
	       (*idx)++;

	return path;
}

#define trans_for_each_path_with_node(_trans, _b, _path, _iter)		\
	for (_iter = 1;							\
	     (_path = __trans_next_path_with_node((_trans), (_b), &_iter));\
	     _iter++)

btree_path_idx_t __bch2_btree_path_make_mut(struct btree_trans *, btree_path_idx_t,
					    bool, unsigned long);

static inline btree_path_idx_t __must_check
bch2_btree_path_make_mut(struct btree_trans *trans,
			 btree_path_idx_t path, bool intent,
			 unsigned long ip)
{
	if (trans->paths[path].ref > 1 ||
	    trans->paths[path].preserve)
		path = __bch2_btree_path_make_mut(trans, path, intent, ip);
	trans->paths[path].should_be_locked = false;
	return path;
}

btree_path_idx_t __must_check
__bch2_btree_path_set_pos(struct btree_trans *, btree_path_idx_t,
			  struct bpos, bool, unsigned long);

static inline btree_path_idx_t __must_check
bch2_btree_path_set_pos(struct btree_trans *trans,
			btree_path_idx_t path, struct bpos new_pos,
			bool intent, unsigned long ip)
{
	return !bpos_eq(new_pos, trans->paths[path].pos)
		? __bch2_btree_path_set_pos(trans, path, new_pos, intent, ip)
		: path;
}

int __must_check bch2_btree_path_traverse_one(struct btree_trans *,
					      btree_path_idx_t,
					      unsigned, unsigned long);

static inline void bch2_trans_verify_not_unlocked_or_in_restart(struct btree_trans *);

static inline int __must_check bch2_btree_path_traverse(struct btree_trans *trans,
					  btree_path_idx_t path, unsigned flags)
{
	bch2_trans_verify_not_unlocked_or_in_restart(trans);

	if (trans->paths[path].uptodate < BTREE_ITER_NEED_RELOCK)
		return 0;

	return bch2_btree_path_traverse_one(trans, path, flags, _RET_IP_);
}

btree_path_idx_t bch2_path_get(struct btree_trans *, enum btree_id, struct bpos,
				 unsigned, unsigned, unsigned, unsigned long);
btree_path_idx_t bch2_path_get_unlocked_mut(struct btree_trans *, enum btree_id,
					    unsigned, struct bpos);

struct bkey_s_c bch2_btree_path_peek_slot(struct btree_path *, struct bkey *);

/*
 * bch2_btree_path_peek_slot() for a cached iterator might return a key in a
 * different snapshot:
 */
static inline struct bkey_s_c bch2_btree_path_peek_slot_exact(struct btree_path *path, struct bkey *u)
{
	struct bkey_s_c k = bch2_btree_path_peek_slot(path, u);

	if (k.k && bpos_eq(path->pos, k.k->p))
		return k;

	bkey_init(u);
	u->p = path->pos;
	return (struct bkey_s_c) { u, NULL };
}

struct bkey_i *bch2_btree_journal_peek_slot(struct btree_trans *,
					struct btree_iter *, struct bpos);

void bch2_btree_path_level_init(struct btree_trans *, struct btree_path *, struct btree *);

int __bch2_trans_mutex_lock(struct btree_trans *, struct mutex *);

static inline int bch2_trans_mutex_lock(struct btree_trans *trans, struct mutex *lock)
{
	return mutex_trylock(lock)
		? 0
		: __bch2_trans_mutex_lock(trans, lock);
}

/* Debug: */

void __bch2_trans_verify_paths(struct btree_trans *);
void __bch2_assert_pos_locked(struct btree_trans *, enum btree_id, struct bpos);

static inline void bch2_trans_verify_paths(struct btree_trans *trans)
{
	if (static_branch_unlikely(&bch2_debug_check_iterators))
		__bch2_trans_verify_paths(trans);
}

static inline void bch2_assert_pos_locked(struct btree_trans *trans, enum btree_id btree,
					  struct bpos pos)
{
	if (static_branch_unlikely(&bch2_debug_check_iterators))
		__bch2_assert_pos_locked(trans, btree, pos);
}

void bch2_btree_path_fix_key_modified(struct btree_trans *trans,
				      struct btree *, struct bkey_packed *);
void bch2_btree_node_iter_fix(struct btree_trans *trans, struct btree_path *,
			      struct btree *, struct btree_node_iter *,
			      struct bkey_packed *, unsigned, unsigned);

int bch2_btree_path_relock_intent(struct btree_trans *, struct btree_path *);

void bch2_path_put(struct btree_trans *, btree_path_idx_t, bool);

int bch2_trans_relock(struct btree_trans *);
int bch2_trans_relock_notrace(struct btree_trans *);
void bch2_trans_unlock(struct btree_trans *);
void bch2_trans_unlock_long(struct btree_trans *);

static inline int trans_was_restarted(struct btree_trans *trans, u32 restart_count)
{
	return restart_count != trans->restart_count
		? -BCH_ERR_transaction_restart_nested
		: 0;
}

void __noreturn bch2_trans_restart_error(struct btree_trans *, u32);

static inline void bch2_trans_verify_not_restarted(struct btree_trans *trans,
						   u32 restart_count)
{
	if (trans_was_restarted(trans, restart_count))
		bch2_trans_restart_error(trans, restart_count);
}

void __noreturn bch2_trans_unlocked_or_in_restart_error(struct btree_trans *);

static inline void bch2_trans_verify_not_unlocked_or_in_restart(struct btree_trans *trans)
{
	if (trans->restarted || !trans->locked)
		bch2_trans_unlocked_or_in_restart_error(trans);
}

__always_inline
static int btree_trans_restart_foreign_task(struct btree_trans *trans, int err, unsigned long ip)
{
	BUG_ON(err <= 0);
	BUG_ON(!bch2_err_matches(-err, BCH_ERR_transaction_restart));

	trans->restarted = err;
	trans->last_restarted_ip = ip;
	return -err;
}

__always_inline
static int btree_trans_restart_ip(struct btree_trans *trans, int err, unsigned long ip)
{
	btree_trans_restart_foreign_task(trans, err, ip);
#ifdef CONFIG_BCACHEFS_DEBUG
	darray_exit(&trans->last_restarted_trace);
	bch2_save_backtrace(&trans->last_restarted_trace, current, 0, GFP_NOWAIT);
#endif
	return -err;
}

__always_inline
static int btree_trans_restart(struct btree_trans *trans, int err)
{
	return btree_trans_restart_ip(trans, err, _THIS_IP_);
}

static inline int trans_maybe_inject_restart(struct btree_trans *trans, unsigned long ip)
{
#ifdef CONFIG_BCACHEFS_INJECT_TRANSACTION_RESTARTS
	if (!(ktime_get_ns() & ~(~0ULL << min(63, (10 + trans->restart_count_this_trans))))) {
		trace_and_count(trans->c, trans_restart_injected, trans, ip);
		return btree_trans_restart_ip(trans,
					BCH_ERR_transaction_restart_fault_inject, ip);
	}
#endif
	return 0;
}

bool bch2_btree_node_upgrade(struct btree_trans *,
			     struct btree_path *, unsigned);

void __bch2_btree_path_downgrade(struct btree_trans *, struct btree_path *, unsigned);

static inline void bch2_btree_path_downgrade(struct btree_trans *trans,
					     struct btree_path *path)
{
	unsigned new_locks_want = path->level + !!path->intent_ref;

	if (path->locks_want > new_locks_want)
		__bch2_btree_path_downgrade(trans, path, new_locks_want);
}

void bch2_trans_downgrade(struct btree_trans *);

void bch2_trans_node_add(struct btree_trans *trans, struct btree_path *, struct btree *);
void bch2_trans_node_drop(struct btree_trans *trans, struct btree *);
void bch2_trans_node_reinit_iter(struct btree_trans *, struct btree *);

int __must_check __bch2_btree_iter_traverse(struct btree_trans *, struct btree_iter *);
int __must_check bch2_btree_iter_traverse(struct btree_trans *, struct btree_iter *);

struct btree *bch2_btree_iter_peek_node(struct btree_trans *, struct btree_iter *);
struct btree *bch2_btree_iter_peek_node_and_restart(struct btree_trans *, struct btree_iter *);
struct btree *bch2_btree_iter_next_node(struct btree_trans *, struct btree_iter *);

struct bkey_s_c bch2_btree_iter_peek_max(struct btree_trans *, struct btree_iter *, struct bpos);
struct bkey_s_c bch2_btree_iter_next(struct btree_trans *, struct btree_iter *);

static inline struct bkey_s_c bch2_btree_iter_peek(struct btree_trans *trans,
						   struct btree_iter *iter)
{
	return bch2_btree_iter_peek_max(trans, iter, SPOS_MAX);
}

struct bkey_s_c bch2_btree_iter_peek_prev_min(struct btree_trans *, struct btree_iter *, struct bpos);

static inline struct bkey_s_c bch2_btree_iter_peek_prev(struct btree_trans *trans, struct btree_iter *iter)
{
	return bch2_btree_iter_peek_prev_min(trans, iter, POS_MIN);
}

struct bkey_s_c bch2_btree_iter_prev(struct btree_trans *, struct btree_iter *);

struct bkey_s_c bch2_btree_iter_peek_slot(struct btree_trans *, struct btree_iter *);
struct bkey_s_c bch2_btree_iter_next_slot(struct btree_trans *, struct btree_iter *);
struct bkey_s_c bch2_btree_iter_prev_slot(struct btree_trans *, struct btree_iter *);

bool bch2_btree_iter_advance(struct btree_trans *, struct btree_iter *);
bool bch2_btree_iter_rewind(struct btree_trans *, struct btree_iter *);

static inline void __bch2_btree_iter_set_pos(struct btree_iter *iter, struct bpos new_pos)
{
	iter->k.type = KEY_TYPE_deleted;
	iter->k.p.inode		= iter->pos.inode	= new_pos.inode;
	iter->k.p.offset	= iter->pos.offset	= new_pos.offset;
	iter->k.p.snapshot	= iter->pos.snapshot	= new_pos.snapshot;
	iter->k.size = 0;
}

static inline void bch2_btree_iter_set_pos(struct btree_trans *trans,
					   struct btree_iter *iter, struct bpos new_pos)
{
	if (unlikely(iter->update_path))
		bch2_path_put(trans, iter->update_path,
			      iter->flags & BTREE_ITER_intent);
	iter->update_path = 0;

	if (!(iter->flags & BTREE_ITER_all_snapshots))
		new_pos.snapshot = iter->snapshot;

	__bch2_btree_iter_set_pos(iter, new_pos);
}

static inline void bch2_btree_iter_set_pos_to_extent_start(struct btree_iter *iter)
{
	BUG_ON(!(iter->flags & BTREE_ITER_is_extents));
	iter->pos = bkey_start_pos(&iter->k);
}

static inline void bch2_btree_iter_set_snapshot(struct btree_trans *trans,
						struct btree_iter *iter, u32 snapshot)
{
	struct bpos pos = iter->pos;

	iter->snapshot = snapshot;
	pos.snapshot = snapshot;
	bch2_btree_iter_set_pos(trans, iter, pos);
}

void bch2_trans_iter_exit(struct btree_trans *, struct btree_iter *);

static inline unsigned bch2_btree_iter_flags(struct btree_trans *trans,
					     unsigned btree_id,
					     unsigned level,
					     unsigned flags)
{
	if (level || !btree_id_cached(trans->c, btree_id)) {
		flags &= ~BTREE_ITER_cached;
		flags &= ~BTREE_ITER_with_key_cache;
	} else if (!(flags & BTREE_ITER_cached))
		flags |= BTREE_ITER_with_key_cache;

	if (!(flags & (BTREE_ITER_all_snapshots|BTREE_ITER_not_extents)) &&
	    btree_id_is_extents(btree_id))
		flags |= BTREE_ITER_is_extents;

	if (!(flags & BTREE_ITER_snapshot_field) &&
	    !btree_type_has_snapshot_field(btree_id))
		flags &= ~BTREE_ITER_all_snapshots;

	if (!(flags & BTREE_ITER_all_snapshots) &&
	    btree_type_has_snapshots(btree_id))
		flags |= BTREE_ITER_filter_snapshots;

	if (trans->journal_replay_not_finished)
		flags |= BTREE_ITER_with_journal;

	return flags;
}

static inline void bch2_trans_iter_init_common(struct btree_trans *trans,
					  struct btree_iter *iter,
					  unsigned btree_id, struct bpos pos,
					  unsigned locks_want,
					  unsigned depth,
					  unsigned flags,
					  unsigned long ip)
{
	iter->update_path	= 0;
	iter->key_cache_path	= 0;
	iter->btree_id		= btree_id;
	iter->min_depth		= 0;
	iter->flags		= flags;
	iter->snapshot		= pos.snapshot;
	iter->pos		= pos;
	iter->k			= POS_KEY(pos);
	iter->journal_idx	= 0;
#ifdef CONFIG_BCACHEFS_DEBUG
	iter->ip_allocated = ip;
#endif
	iter->path = bch2_path_get(trans, btree_id, iter->pos,
				   locks_want, depth, flags, ip);
}

void bch2_trans_iter_init_outlined(struct btree_trans *, struct btree_iter *,
			  enum btree_id, struct bpos, unsigned);

static inline void bch2_trans_iter_init(struct btree_trans *trans,
			  struct btree_iter *iter,
			  unsigned btree_id, struct bpos pos,
			  unsigned flags)
{
	if (__builtin_constant_p(btree_id) &&
	    __builtin_constant_p(flags))
		bch2_trans_iter_init_common(trans, iter, btree_id, pos, 0, 0,
				bch2_btree_iter_flags(trans, btree_id, 0, flags),
				_THIS_IP_);
	else
		bch2_trans_iter_init_outlined(trans, iter, btree_id, pos, flags);
}

void bch2_trans_node_iter_init(struct btree_trans *, struct btree_iter *,
			       enum btree_id, struct bpos,
			       unsigned, unsigned, unsigned);
void bch2_trans_copy_iter(struct btree_trans *, struct btree_iter *, struct btree_iter *);

void bch2_set_btree_iter_dontneed(struct btree_trans *, struct btree_iter *);

#ifdef CONFIG_BCACHEFS_TRANS_KMALLOC_TRACE
void bch2_trans_kmalloc_trace_to_text(struct printbuf *,
				      darray_trans_kmalloc_trace *);
#endif

void *__bch2_trans_kmalloc(struct btree_trans *, size_t, unsigned long);

static inline void bch2_trans_kmalloc_trace(struct btree_trans *trans, size_t size,
					    unsigned long ip)
{
#ifdef CONFIG_BCACHEFS_TRANS_KMALLOC_TRACE
	darray_push(&trans->trans_kmalloc_trace,
		    ((struct trans_kmalloc_trace) { .ip = ip, .bytes = size }));
#endif
}

static __always_inline void *bch2_trans_kmalloc_nomemzero_ip(struct btree_trans *trans, size_t size,
						    unsigned long ip)
{
	size = roundup(size, 8);

	bch2_trans_kmalloc_trace(trans, size, ip);

	if (likely(trans->mem_top + size <= trans->mem_bytes)) {
		void *p = trans->mem + trans->mem_top;

		trans->mem_top += size;
		return p;
	} else {
		return __bch2_trans_kmalloc(trans, size, ip);
	}
}

static __always_inline void *bch2_trans_kmalloc_ip(struct btree_trans *trans, size_t size,
					  unsigned long ip)
{
	size = roundup(size, 8);

	bch2_trans_kmalloc_trace(trans, size, ip);

	if (likely(trans->mem_top + size <= trans->mem_bytes)) {
		void *p = trans->mem + trans->mem_top;

		trans->mem_top += size;
		memset(p, 0, size);
		return p;
	} else {
		return __bch2_trans_kmalloc(trans, size, ip);
	}
}

/**
 * bch2_trans_kmalloc - allocate memory for use by the current transaction
 *
 * Must be called after bch2_trans_begin, which on second and further calls
 * frees all memory allocated in this transaction
 */
static __always_inline void *bch2_trans_kmalloc(struct btree_trans *trans, size_t size)
{
	return bch2_trans_kmalloc_ip(trans, size, _THIS_IP_);
}

static __always_inline void *bch2_trans_kmalloc_nomemzero(struct btree_trans *trans, size_t size)
{
	return bch2_trans_kmalloc_nomemzero_ip(trans, size, _THIS_IP_);
}

static inline struct bkey_s_c __bch2_bkey_get_iter(struct btree_trans *trans,
				struct btree_iter *iter,
				unsigned btree_id, struct bpos pos,
				unsigned flags, unsigned type)
{
	struct bkey_s_c k;

	bch2_trans_iter_init(trans, iter, btree_id, pos, flags);
	k = bch2_btree_iter_peek_slot(trans, iter);

	if (!bkey_err(k) && type && k.k->type != type)
		k = bkey_s_c_err(-BCH_ERR_ENOENT_bkey_type_mismatch);
	if (unlikely(bkey_err(k)))
		bch2_trans_iter_exit(trans, iter);
	return k;
}

static inline struct bkey_s_c bch2_bkey_get_iter(struct btree_trans *trans,
				struct btree_iter *iter,
				unsigned btree_id, struct bpos pos,
				unsigned flags)
{
	return __bch2_bkey_get_iter(trans, iter, btree_id, pos, flags, 0);
}

#define bch2_bkey_get_iter_typed(_trans, _iter, _btree_id, _pos, _flags, _type)\
	bkey_s_c_to_##_type(__bch2_bkey_get_iter(_trans, _iter,			\
				       _btree_id, _pos, _flags, KEY_TYPE_##_type))

static inline void __bkey_val_copy(void *dst_v, unsigned dst_size, struct bkey_s_c src_k)
{
	unsigned b = min_t(unsigned, dst_size, bkey_val_bytes(src_k.k));
	memcpy(dst_v, src_k.v, b);
	if (unlikely(b < dst_size))
		memset(dst_v + b, 0, dst_size - b);
}

#define bkey_val_copy(_dst_v, _src_k)					\
do {									\
	BUILD_BUG_ON(!__typecheck(*_dst_v, *_src_k.v));			\
	__bkey_val_copy(_dst_v, sizeof(*_dst_v), _src_k.s_c);		\
} while (0)

static inline int __bch2_bkey_get_val_typed(struct btree_trans *trans,
				unsigned btree_id, struct bpos pos,
				unsigned flags, unsigned type,
				unsigned val_size, void *val)
{
	struct btree_iter iter;
	struct bkey_s_c k = __bch2_bkey_get_iter(trans, &iter, btree_id, pos, flags, type);
	int ret = bkey_err(k);
	if (!ret) {
		__bkey_val_copy(val, val_size, k);
		bch2_trans_iter_exit(trans, &iter);
	}

	return ret;
}

#define bch2_bkey_get_val_typed(_trans, _btree_id, _pos, _flags, _type, _val)\
	__bch2_bkey_get_val_typed(_trans, _btree_id, _pos, _flags,	\
				  KEY_TYPE_##_type, sizeof(*_val), _val)

void bch2_trans_srcu_unlock(struct btree_trans *);

u32 bch2_trans_begin(struct btree_trans *);

#define __for_each_btree_node(_trans, _iter, _btree_id, _start,			\
			      _locks_want, _depth, _flags, _b, _do)		\
({										\
	bch2_trans_begin((_trans));						\
										\
	struct btree_iter _iter;						\
	bch2_trans_node_iter_init((_trans), &_iter, (_btree_id),		\
				  _start, _locks_want, _depth, _flags);		\
	int _ret3 = 0;								\
	do {									\
		_ret3 = lockrestart_do((_trans), ({				\
			struct btree *_b = bch2_btree_iter_peek_node(_trans, &_iter);\
			if (!_b)						\
				break;						\
										\
			PTR_ERR_OR_ZERO(_b) ?: (_do);				\
		})) ?:								\
		lockrestart_do((_trans),					\
			PTR_ERR_OR_ZERO(bch2_btree_iter_next_node(_trans, &_iter)));\
	} while (!_ret3);							\
										\
	bch2_trans_iter_exit((_trans), &(_iter));				\
	_ret3;									\
})

#define for_each_btree_node(_trans, _iter, _btree_id, _start,		\
			    _flags, _b, _do)				\
	__for_each_btree_node(_trans, _iter, _btree_id, _start,	\
			      0, 0, _flags, _b, _do)

static inline struct bkey_s_c bch2_btree_iter_peek_prev_type(struct btree_trans *trans,
							     struct btree_iter *iter,
							     unsigned flags)
{
	return  flags & BTREE_ITER_slots      ? bch2_btree_iter_peek_slot(trans, iter) :
						bch2_btree_iter_peek_prev(trans, iter);
}

static inline struct bkey_s_c bch2_btree_iter_peek_type(struct btree_trans *trans,
							struct btree_iter *iter,
							unsigned flags)
{
	return  flags & BTREE_ITER_slots      ? bch2_btree_iter_peek_slot(trans, iter) :
						bch2_btree_iter_peek(trans, iter);
}

static inline struct bkey_s_c bch2_btree_iter_peek_max_type(struct btree_trans *trans,
							    struct btree_iter *iter,
							    struct bpos end,
							    unsigned flags)
{
	if (!(flags & BTREE_ITER_slots))
		return bch2_btree_iter_peek_max(trans, iter, end);

	if (bkey_gt(iter->pos, end))
		return bkey_s_c_null;

	return bch2_btree_iter_peek_slot(trans, iter);
}

int __bch2_btree_trans_too_many_iters(struct btree_trans *);

static inline int btree_trans_too_many_iters(struct btree_trans *trans)
{
	if (bitmap_weight(trans->paths_allocated, trans->nr_paths) > BTREE_ITER_NORMAL_LIMIT - 8)
		return __bch2_btree_trans_too_many_iters(trans);

	return 0;
}

/*
 * goto instead of loop, so that when used inside for_each_btree_key2()
 * break/continue work correctly
 */
#define lockrestart_do(_trans, _do)					\
({									\
	__label__ transaction_restart;					\
	u32 _restart_count;						\
	int _ret2;							\
transaction_restart:							\
	_restart_count = bch2_trans_begin(_trans);			\
	_ret2 = (_do);							\
									\
	if (bch2_err_matches(_ret2, BCH_ERR_transaction_restart))	\
		goto transaction_restart;				\
									\
	if (!_ret2)							\
		bch2_trans_verify_not_restarted(_trans, _restart_count);\
	_ret2;								\
})

/*
 * nested_lockrestart_do(), nested_commit_do():
 *
 * These are like lockrestart_do() and commit_do(), with two differences:
 *
 *  - We don't call bch2_trans_begin() unless we had a transaction restart
 *  - We return -BCH_ERR_transaction_restart_nested if we succeeded after a
 *  transaction restart
 */
#define nested_lockrestart_do(_trans, _do)				\
({									\
	u32 _restart_count, _orig_restart_count;			\
	int _ret2;							\
									\
	_restart_count = _orig_restart_count = (_trans)->restart_count;	\
									\
	while (bch2_err_matches(_ret2 = (_do), BCH_ERR_transaction_restart))\
		_restart_count = bch2_trans_begin(_trans);		\
									\
	if (!_ret2)							\
		bch2_trans_verify_not_restarted(_trans, _restart_count);\
									\
	_ret2 ?: trans_was_restarted(_trans, _orig_restart_count);		\
})

#define for_each_btree_key_max_continue(_trans, _iter,			\
					 _end, _flags, _k, _do)		\
({									\
	struct bkey_s_c _k;						\
	int _ret3 = 0;							\
									\
	do {								\
		_ret3 = lockrestart_do(_trans, ({			\
			(_k) = bch2_btree_iter_peek_max_type(_trans, &(_iter),	\
						_end, (_flags));	\
			if (!(_k).k)					\
				break;					\
									\
			bkey_err(_k) ?: (_do);				\
		}));							\
	} while (!_ret3 && bch2_btree_iter_advance(_trans, &(_iter)));	\
									\
	bch2_trans_iter_exit((_trans), &(_iter));			\
	_ret3;								\
})

#define for_each_btree_key_continue(_trans, _iter, _flags, _k, _do)	\
	for_each_btree_key_max_continue(_trans, _iter, SPOS_MAX, _flags, _k, _do)

#define for_each_btree_key_max(_trans, _iter, _btree_id,		\
				_start, _end, _flags, _k, _do)		\
({									\
	bch2_trans_begin(trans);					\
									\
	struct btree_iter _iter;					\
	bch2_trans_iter_init((_trans), &(_iter), (_btree_id),		\
			     (_start), (_flags));			\
									\
	for_each_btree_key_max_continue(_trans, _iter, _end, _flags, _k, _do);\
})

#define for_each_btree_key(_trans, _iter, _btree_id,			\
			   _start, _flags, _k, _do)			\
	for_each_btree_key_max(_trans, _iter, _btree_id, _start,	\
				 SPOS_MAX, _flags, _k, _do)

#define for_each_btree_key_reverse(_trans, _iter, _btree_id,		\
				   _start, _flags, _k, _do)		\
({									\
	struct btree_iter _iter;					\
	struct bkey_s_c _k;						\
	int _ret3 = 0;							\
									\
	bch2_trans_iter_init((_trans), &(_iter), (_btree_id),		\
			     (_start), (_flags));			\
									\
	do {								\
		_ret3 = lockrestart_do(_trans, ({			\
			(_k) = bch2_btree_iter_peek_prev_type(_trans, &(_iter),	\
							(_flags));	\
			if (!(_k).k)					\
				break;					\
									\
			bkey_err(_k) ?: (_do);				\
		}));							\
	} while (!_ret3 && bch2_btree_iter_rewind(_trans, &(_iter)));	\
									\
	bch2_trans_iter_exit((_trans), &(_iter));			\
	_ret3;								\
})

#define for_each_btree_key_commit(_trans, _iter, _btree_id,		\
				  _start, _iter_flags, _k,		\
				  _disk_res, _journal_seq, _commit_flags,\
				  _do)					\
	for_each_btree_key(_trans, _iter, _btree_id, _start, _iter_flags, _k,\
			    (_do) ?: bch2_trans_commit(_trans, (_disk_res),\
					(_journal_seq), (_commit_flags)))

#define for_each_btree_key_reverse_commit(_trans, _iter, _btree_id,	\
				  _start, _iter_flags, _k,		\
				  _disk_res, _journal_seq, _commit_flags,\
				  _do)					\
	for_each_btree_key_reverse(_trans, _iter, _btree_id, _start, _iter_flags, _k,\
			    (_do) ?: bch2_trans_commit(_trans, (_disk_res),\
					(_journal_seq), (_commit_flags)))

#define for_each_btree_key_max_commit(_trans, _iter, _btree_id,	\
				  _start, _end, _iter_flags, _k,	\
				  _disk_res, _journal_seq, _commit_flags,\
				  _do)					\
	for_each_btree_key_max(_trans, _iter, _btree_id, _start, _end, _iter_flags, _k,\
			    (_do) ?: bch2_trans_commit(_trans, (_disk_res),\
					(_journal_seq), (_commit_flags)))

struct bkey_s_c bch2_btree_iter_peek_and_restart_outlined(struct btree_trans *,
							  struct btree_iter *);

#define for_each_btree_key_max_norestart(_trans, _iter, _btree_id,	\
			   _start, _end, _flags, _k, _ret)		\
	for (bch2_trans_iter_init((_trans), &(_iter), (_btree_id),	\
				  (_start), (_flags));			\
	     (_k) = bch2_btree_iter_peek_max_type(_trans, &(_iter), _end, _flags),\
	     !((_ret) = bkey_err(_k)) && (_k).k;			\
	     bch2_btree_iter_advance(_trans, &(_iter)))

#define for_each_btree_key_max_continue_norestart(_trans, _iter, _end, _flags, _k, _ret)\
	for (;									\
	     (_k) = bch2_btree_iter_peek_max_type(_trans, &(_iter), _end, _flags),	\
	     !((_ret) = bkey_err(_k)) && (_k).k;				\
	     bch2_btree_iter_advance(_trans, &(_iter)))

#define for_each_btree_key_norestart(_trans, _iter, _btree_id,		\
			   _start, _flags, _k, _ret)			\
	for_each_btree_key_max_norestart(_trans, _iter, _btree_id, _start,\
					  SPOS_MAX, _flags, _k, _ret)

#define for_each_btree_key_reverse_norestart(_trans, _iter, _btree_id,		\
					     _start, _flags, _k, _ret)		\
	for (bch2_trans_iter_init((_trans), &(_iter), (_btree_id),		\
				  (_start), (_flags));				\
	     (_k) = bch2_btree_iter_peek_prev_type(_trans, &(_iter), _flags),	\
	     !((_ret) = bkey_err(_k)) && (_k).k;				\
	     bch2_btree_iter_rewind(_trans, &(_iter)))

#define for_each_btree_key_continue_norestart(_trans, _iter, _flags, _k, _ret)	\
	for_each_btree_key_max_continue_norestart(_trans, _iter, SPOS_MAX, _flags, _k, _ret)

/*
 * This should not be used in a fastpath, without first trying _do in
 * nonblocking mode - it will cause excessive transaction restarts and
 * potentially livelocking:
 */
#define drop_locks_do(_trans, _do)					\
({									\
	bch2_trans_unlock(_trans);					\
	(_do) ?: bch2_trans_relock(_trans);				\
})

#define allocate_dropping_locks_errcode(_trans, _do)			\
({									\
	gfp_t _gfp = GFP_NOWAIT|__GFP_NOWARN;				\
	int _ret = _do;							\
									\
	if (bch2_err_matches(_ret, ENOMEM)) {				\
		_gfp = GFP_KERNEL;					\
		_ret = drop_locks_do(_trans, _do);			\
	}								\
	_ret;								\
})

#define allocate_dropping_locks(_trans, _ret, _do)			\
({									\
	gfp_t _gfp = GFP_NOWAIT|__GFP_NOWARN;				\
	typeof(_do) _p = _do;						\
									\
	_ret = 0;							\
	if (unlikely(!_p)) {						\
		_gfp = GFP_KERNEL;					\
		_ret = drop_locks_do(_trans, ((_p = _do), 0));		\
	}								\
	_p;								\
})

#define allocate_dropping_locks_norelock(_trans, _lock_dropped, _do)	\
({									\
	gfp_t _gfp = GFP_NOWAIT|__GFP_NOWARN;				\
	typeof(_do) _p = _do;						\
	_lock_dropped = false;						\
	if (unlikely(!_p)) {						\
		bch2_trans_unlock(_trans);				\
		_lock_dropped = true;					\
		_gfp = GFP_KERNEL;					\
		_p = _do;						\
	}								\
	_p;								\
})

struct btree_trans *__bch2_trans_get(struct bch_fs *, unsigned);
void bch2_trans_put(struct btree_trans *);

bool bch2_current_has_btree_trans(struct bch_fs *);

extern const char *bch2_btree_transaction_fns[BCH_TRANSACTIONS_NR];
unsigned bch2_trans_get_fn_idx(const char *);

#define bch2_trans_get(_c)						\
({									\
	static unsigned trans_fn_idx;					\
									\
	if (unlikely(!trans_fn_idx))					\
		trans_fn_idx = bch2_trans_get_fn_idx(__func__);		\
	__bch2_trans_get(_c, trans_fn_idx);				\
})

/*
 * We don't use DEFINE_CLASS() because using a function for the constructor
 * breaks bch2_trans_get()'s use of __func__
 */
typedef struct btree_trans * class_btree_trans_t;
static inline void class_btree_trans_destructor(struct btree_trans **p)
{
	struct btree_trans *trans = *p;
	bch2_trans_put(trans);
}

#define class_btree_trans_constructor(_c)	bch2_trans_get(_c)

/* deprecated, prefer CLASS(btree_trans) */
#define bch2_trans_run(_c, _do)						\
({									\
	CLASS(btree_trans, trans)(_c);					\
	(_do);								\
})

/* deprecated, prefer CLASS(btree_trans) */
#define bch2_trans_do(_c, _do)						\
({									\
	CLASS(btree_trans, trans)(_c);					\
	lockrestart_do(trans, _do);					\
})

void bch2_btree_trans_to_text(struct printbuf *, struct btree_trans *);

void bch2_fs_btree_iter_exit(struct bch_fs *);
void bch2_fs_btree_iter_init_early(struct bch_fs *);
int bch2_fs_btree_iter_init(struct bch_fs *);

#endif /* _BCACHEFS_BTREE_ITER_H */
