/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_BTREE_ITER_H
#define _BCACHEFS_BTREE_ITER_H

#include "btree/bset.h"
#include "btree/cache.h"
#include "btree/node/bset_gen.h"
#include "btree/node/iter_gen.h"
#include "btree/path_gen.h"
#include "btree/traverse_gen.h"
#include "btree/types_gen.h"
#include "btree/node_gen.h"
#include "btree/locking_gen.h"

#include "closure.h"
#include "sb/counters.h"

static inline int __bkey_err(const struct bkey *k)
{
	return PTR_ERR_OR_ZERO(k);
}

#define bkey_err(_k)	__bkey_err((_k).k)

#define bkey_try(_do)					\
({							\
	typeof(_do) _k = _do;				\
	try(bkey_err(_k));				\
	_k;						\
})

/*
 * btree_paths_realloc() poisons the node pointers in the array it retires, so
 * a path that outlived a realloc reads this rather than a plausible-looking
 * stale node.
 *
 * Check it where a path pointer arrives FROM A CALLER. Anything reached by
 * walking trans->paths is live by construction - bch2_btree_path_verify() and
 * friends can never see this - so the entry points are the only places a stale
 * path can be caught.
 */
#define BTREE_PATH_POISON	ERR_PTR(-BCH_ERR_no_btree_node_stale_paths)

static inline bool btree_path_poisoned(const struct btree *b)
{
	return b == BTREE_PATH_POISON;
}

static inline struct btree *btree_path_node(struct btree_path *path,
					    unsigned level)
{
	if (level >= BTREE_MAX_DEPTH)
		return NULL;

	struct btree *b = path->l[level].b;

#ifdef CONFIG_BCACHEFS_DEBUG
	/*
	 * Spot check, deliberately not exhaustive: most level accesses go
	 * direct to path->l[] rather than through here. Sites that dereference
	 * b fault on the poison unaided; this catches some of the ones that
	 * only compare it, which otherwise read as "not the node I wanted" and
	 * take a plausible wrong branch.
	 */
	WARN_ONCE(btree_path_poisoned(b),
		  "using a btree path that outlived trans->paths\n");
#endif
	return b;
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

#define trans_for_each_path_idx_inorder(_trans, _iter)			\
	for (_iter = (struct trans_for_each_path_inorder_iter) { 0 };	\
	     (_iter.path_idx = trans->sorted[_iter.sorted_idx],		\
	      _iter.sorted_idx < (_trans)->nr_sorted);			\
	     _iter.sorted_idx++)

#include "btree/iter_gen.h"
#include "btree/trans_gen.h"

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

static inline btree_path_idx_t __must_check
bch2_btree_path_set_pos(struct btree_trans *trans,
			btree_path_idx_t path,
			const struct bpos *new_pos,
			bool intent, unsigned long ip)
{
	return !bpos_eq(*new_pos, trans->paths[path].pos)
		? __bch2_btree_path_set_pos(trans, path, new_pos, intent, ip)
		: path;
}

static inline void bch2_trans_verify_not_unlocked_or_in_restart(struct btree_trans *);

static inline int bch2_trans_mutex_lock(struct btree_trans *trans, struct mutex *lock)
{
	return mutex_trylock(lock)
		? 0
		: __bch2_trans_mutex_lock(trans, lock);
}

/* Debug: */

static inline int bch2_trans_relock(struct btree_trans *trans)
{
	return trans->locked && !trans->restarted
		? 0
		: __bch2_trans_relock(trans, true);
}

/* bch2_trans_srcu_wait_budget(), for a wait that may have no trans */
static inline long bch2_trans_short_wait_budget(struct btree_trans *trans, long timeout)
{
	return trans ? bch2_trans_srcu_wait_budget(trans, timeout) : timeout;
}

/*
 * SRCU-aware wrappers around closure_sync{,_timeout}: short waits under SRCU
 * until the budget is exhausted, then drop SRCU and wait the remainder.
 *
 * trans_closure_sync_timeout() returns 0 on success (closure drained),
 * -ETIME if timeout fully elapsed.
 */
static inline int trans_closure_sync_timeout(struct btree_trans *trans,
					     struct closure *cl,
					     long timeout)
{
	long remaining = timeout;

	while (remaining > 0) {
		if (closure_nr_remaining(cl) <= 1)
			return 0;

		unsigned long start = jiffies;
		long wait = bch2_trans_short_wait_budget(trans, remaining);

		if (!closure_sync_timeout(cl, wait))
			return 0;
		remaining -= (long)(jiffies - start);
	}
	return -ETIME;
}

/*
 * Wrappers around closure_wait_event{,_timeout} that drop SRCU before
 * blocking long. Use these instead of closure_wait_event() directly when a
 * btree_trans is in scope.
 *
 * Mirrors closure_wait_event_timeout(); the only change is that the inner
 * sync uses trans_closure_sync_timeout() so the SRCU short-wait budget is
 * honoured on each blocking pass.
 */
#define __trans_wait_event_timeout(_trans, waitlist, _cond, _until)		\
({										\
	CLASS(closure_stack, cl)();						\
	long _t;								\
										\
	while (1) {								\
		bch2_closure_wait(waitlist, &cl);				\
		if (_cond) {							\
			_t = max_t(long, 1L, _until - jiffies);			\
			break;							\
		}								\
		_t = max_t(long, 0L, _until - jiffies);				\
		if (!_t)							\
			break;							\
		trans_closure_sync_timeout(_trans, &cl, _t);			\
	}									\
	closure_wake_up(waitlist);						\
	_t;									\
})

#define trans_wait_event_timeout(_trans, waitlist, _cond, _timeout)		\
({										\
	unsigned long _until = jiffies + (_timeout);				\
	(_cond)									\
		? max_t(long, 1L, _until - jiffies)				\
		: __trans_wait_event_timeout(_trans, waitlist, _cond, _until);	\
})

#define trans_wait_event(_trans, _waitlist, _condition)				\
	trans_wait_event_timeout(_trans, _waitlist, _condition, MAX_SCHEDULE_TIMEOUT)

static inline int trans_was_restarted(struct btree_trans *trans, u32 restart_count)
{
	return restart_count != trans->restart_count
		? -BCH_ERR_transaction_restart_nested
		: 0;
}

static inline void bch2_trans_verify_not_restarted(struct btree_trans *trans,
						   u32 restart_count)
{
	if (trans_was_restarted(trans, restart_count))
		bch2_trans_restart_error(trans, restart_count);
}

static inline void bch2_trans_verify_not_unlocked_or_in_restart(struct btree_trans *trans)
{
#ifdef CONFIG_BCACHEFS_DEBUG
	if (trans->restarted || !trans->locked)
		bch2_trans_unlocked_or_in_restart_error(trans);
#endif
}

__always_inline
static int btree_trans_restart(struct btree_trans *trans, int err)
{
	return bch2_trans_restart_ip(trans, err, _THIS_IP_);
}

static inline int trans_maybe_inject_restart(struct btree_trans *trans, unsigned long ip)
{
#ifdef CONFIG_BCACHEFS_INJECT_TRANSACTION_RESTARTS
	return __bch2_trans_maybe_inject_restart(trans, ip);
#else
	return 0;
#endif
}

static inline struct bkey_s_c bch2_btree_iter_peek(struct btree_iter *iter)
{
	return bch2_btree_iter_peek_max(iter, &SPOS_MAX);
}

/* BTREE_ITER_trim is for extents, and ignored on any other iterator: */
static inline bool bch2_btree_iter_trims(const struct btree_iter *iter)
{
	return (iter->flags & (BTREE_ITER_trim|BTREE_ITER_is_extents)) ==
		(BTREE_ITER_trim|BTREE_ITER_is_extents);
}

static inline struct bkey_s_c bch2_btree_iter_peek_prev(struct btree_iter *iter)
{
	return bch2_btree_iter_peek_prev_min(iter, POS_MIN);
}

static inline void __bch2_btree_iter_set_pos(struct btree_iter *iter, struct bpos new_pos)
{
	iter->k.type = KEY_TYPE_deleted;
	iter->k.p.inode		= iter->pos.inode	= new_pos.inode;
	iter->k.p.offset	= iter->pos.offset	= new_pos.offset;
	iter->k.p.snapshot	= iter->pos.snapshot	= new_pos.snapshot;
	iter->k.size = 0;
}

static inline void bch2_btree_iter_set_pos(struct btree_iter *iter, struct bpos new_pos)
{
	struct btree_trans *trans = iter->trans;

	if (unlikely(iter->update_path))
		bch2_path_put(trans, iter->update_path,
			      iter->flags & BTREE_ITER_intent);
	iter->update_path = 0;

	if (!(iter->flags & BTREE_ITER_all_snapshots))
		new_pos.snapshot = iter->snapshot;

	__bch2_btree_iter_set_pos(iter, new_pos);
}

static inline void bch2_btree_iter_set_snapshot(struct btree_iter *iter, u32 snapshot)
{
	struct bpos pos = iter->pos;

	iter->snapshot = snapshot;
	pos.snapshot = snapshot;
	bch2_btree_iter_set_pos(iter, pos);
}

static inline bool btree_id_cached(enum btree_id btree)
{
	return BIT_ULL(btree) &
		(BIT_ULL(BTREE_ID_alloc)|
		 BIT_ULL(BTREE_ID_inodes)|
		 BIT_ULL(BTREE_ID_logged_ops)|
		 BIT_ULL(BTREE_ID_stripes)|
		 BIT_ULL(BTREE_ID_subvolumes));
}

static inline void __bch2_trans_iter_init(struct btree_trans *trans,
			  struct btree_iter *iter,
			  enum btree_id btree, struct bpos pos,
			  enum btree_iter_update_trigger_flags flags)
{
	bch2_trans_iter_init_outlined(trans, iter, btree, pos, flags, _THIS_IP_);
}

static inline void bch2_trans_iter_init(struct btree_trans *trans,
			  struct btree_iter *iter,
			  enum btree_id btree, struct bpos pos,
			  enum btree_iter_update_trigger_flags flags)
{
	bch2_trans_iter_exit(iter);
	__bch2_trans_iter_init(trans, iter, btree, pos, flags);
}

#define DEFINE_CLASS2(_name, _type, _exit, _init, _init_args...)		\
typedef _type class_##_name##_t;					\
static __always_inline void class_##_name##_destructor(_type *p)			\
{ _type _T = *p; _exit; }						\
static __always_inline _type class_##_name##_constructor(_init_args)		\
{ _type t = _init; return t; }

#define bch2_trans_iter_class_init(_trans, _btree, _pos, _flags)		\
({										\
	struct btree_iter iter;							\
	__bch2_trans_iter_init(_trans, &iter, (_btree), (_pos), (_flags));	\
	iter;									\
})

DEFINE_CLASS2(btree_iter, struct btree_iter,
	     bch2_trans_iter_exit(&_T),
	     bch2_trans_iter_class_init(trans, btree, pos, flags),
	     struct btree_trans *trans,
	     enum btree_id btree, struct bpos pos,
	     enum btree_iter_update_trigger_flags flags);

#define bch2_trans_iter_uninit_class_init()					\
({										\
	struct btree_iter iter = {};						\
	iter;									\
})

DEFINE_CLASS(btree_iter_uninit, struct btree_iter,
	     bch2_trans_iter_exit(&_T),
	     bch2_trans_iter_uninit_class_init(),
	     struct btree_trans *trans)

#define bch2_trans_iter_copy_class_init(_src)					\
({										\
	struct btree_iter iter = {};						\
	bch2_trans_copy_iter(&iter, _src);					\
	iter;									\
})

DEFINE_CLASS(btree_iter_copy, struct btree_iter,
	     bch2_trans_iter_exit(&_T),
	     bch2_trans_iter_copy_class_init(src),
	     struct btree_iter *src)

#define bch2_trans_iter_ll_class_init(_trans, _btree, _pos, _locks_want, _depth, _flags)\
({										\
	struct btree_iter iter;							\
	__bch2_trans_iter_init_ll(_trans, &iter, (_btree), (_pos),		\
				  (_locks_want), (_depth), (_flags), _THIS_IP_);\
	iter;									\
})

DEFINE_CLASS(btree_iter_ll, struct btree_iter,
	     bch2_trans_iter_exit(&_T),
	     bch2_trans_iter_ll_class_init(trans, btree, pos, locks_want, depth, flags),
	     struct btree_trans *trans, enum btree_id btree, struct bpos pos,
	     unsigned locks_want, unsigned depth,
	     enum btree_iter_update_trigger_flags flags)

#define bch2_trans_node_iter_class_init(_trans, _btree, _pos, _locks_want, _depth, _flags)\
({										\
	struct btree_iter iter;							\
	__bch2_trans_node_iter_init(_trans, &iter, (_btree), (_pos),		\
				    (_locks_want), (_depth), (_flags));		\
	iter;									\
})

DEFINE_CLASS(btree_node_iter, struct btree_iter,
	     bch2_trans_iter_exit(&_T),
	     bch2_trans_node_iter_class_init(trans, btree, pos, locks_want, depth, flags),
	     struct btree_trans *trans,
	     enum btree_id btree, struct bpos pos,
	     unsigned locks_want, unsigned depth,
	     enum btree_iter_update_trigger_flags flags);


#ifdef CONFIG_BCACHEFS_TRANS_KMALLOC_TRACE
void bch2_trans_kmalloc_trace_to_text(struct printbuf *,
				      darray_trans_kmalloc_trace *);
#endif

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

	if (likely(trans->mem.nr + size / sizeof(u64) <= trans->mem.size)) {
		void *p = trans->mem.data + trans->mem.nr;

		trans->mem.nr += size / sizeof(u64);
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

	if (likely(trans->mem.nr + size / sizeof(u64) <= trans->mem.size)) {
		void *p = trans->mem.data + trans->mem.nr;

		trans->mem.nr += size / sizeof(u64);
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

static inline struct bkey_s_c __bch2_bkey_get_typed(struct btree_iter *iter,
						    enum bch_bkey_type type)
{
	struct bkey_s_c k = bch2_btree_iter_peek_slot(iter);

	if (!bkey_err(k) && type && k.k->type != type)
		k = bkey_s_c_err(bch_err_throw(iter->trans->c, ENOENT_bkey_type_mismatch));
	return k;
}

#define bch2_bkey_get_typed(_iter, _type)						\
	bkey_s_c_to_##_type(__bch2_bkey_get_typed(_iter, KEY_TYPE_##_type))

/*
 * Copy a value out of a key, zeroing whatever the key is too short to hold.
 *
 * Values grow: a key written by an older version stops short of the fields
 * added since, and we have no defaults, so a field the key predates reads as
 * 0. To ask whether the key was written with a field at all, test
 * bkey_val_bytes() against offsetof() - that's the only thing that answers it,
 * and it's what to_text() and validate() use.
 *
 * The typed mut helpers pass min_bytes = sizeof(struct bkey_i_<type>) and
 * widen u64s to match, so a caller may assign a field the on-disk key was too
 * short for and have it committed - see __bch2_bkey_make_mut_noupdate().
 */
static inline void __bkey_val_copy_pad(void *dst_v, unsigned dst_size, struct bkey_s_c src_k)
{
	unsigned b = min_t(unsigned, dst_size, bkey_val_bytes(src_k.k));
	/*
	 * __bch2_bkey_get_i_typed() passes &bkey_i->v, and struct bch_val is
	 * zero length: the bytes belong to the caller's bkey_i_<type>, which
	 * fortify cannot see through a struct bkey_i pointer. b is clamped to
	 * dst_size above, so the write stays within what the caller declared.
	 */
	unsafe_memcpy(dst_v, src_k.v, b, "bkey value, sized by the caller");
	if (unlikely(b < dst_size))
		memset(dst_v + b, 0, dst_size - b);
}

#define bkey_val_copy_pad(_dst_v, _src_k)				\
do {									\
	BUILD_BUG_ON(!__typecheck(*_dst_v, *_src_k.v));			\
	__bkey_val_copy_pad(_dst_v, sizeof(*_dst_v), _src_k.s_c);	\
} while (0)

static inline int __bch2_bkey_get_val_typed(struct btree_trans *trans,
				enum btree_id btree, struct bpos pos,
				enum btree_iter_update_trigger_flags flags,
				enum bch_bkey_type type,
				unsigned val_size, void *val)
{
	CLASS(btree_iter, iter)(trans, btree, pos, flags);
	struct bkey_s_c k = __bch2_bkey_get_typed(&iter, type);
	int ret = bkey_err(k);
	if (!ret)
		__bkey_val_copy_pad(val, val_size, k);
	return ret;
}

#define bch2_bkey_get_val_typed(_trans, _btree_id, _pos, _flags, _type, _val)\
	__bch2_bkey_get_val_typed(_trans, _btree_id, _pos, _flags,	\
				  KEY_TYPE_##_type, sizeof(*_val), _val)

/*
 * As bch2_bkey_get_val_typed(), but fetches the whole key into a caller-stack
 * bkey_i_<type>, so an error message can print the key instead of the fields
 * whoever wrote the message happened to name.
 *
 * u64s keeps the on-disk length: the key is never written back, and
 * to_text() reports u64s and gates fields on it, so widening would print
 * fields the key doesn't have. Clamping to val_size is required in the other
 * direction - an on-disk val longer than the caller's struct would leave u64s
 * claiming val we didn't copy, and to_text() would read past the stack.
 */
static inline int __bch2_bkey_get_i_typed(struct btree_trans *trans,
				enum btree_id btree, struct bpos pos,
				enum btree_iter_update_trigger_flags flags,
				enum bch_bkey_type type,
				unsigned val_size, struct bkey_i *dst)
{
	CLASS(btree_iter, iter)(trans, btree, pos, flags);
	struct bkey_s_c k = __bch2_bkey_get_typed(&iter, type);
	int ret = bkey_err(k);
	if (!ret) {
		dst->k = *k.k;
		set_bkey_val_bytes(&dst->k, min_t(unsigned, val_size, bkey_val_bytes(k.k)));
		__bkey_val_copy_pad(&dst->v, val_size, k);
	}
	return ret;
}

#define bch2_bkey_get_i_typed(_trans, _btree_id, _pos, _flags, _type, _k)\
	__bch2_bkey_get_i_typed(_trans, _btree_id, _pos, _flags,		\
				KEY_TYPE_##_type, sizeof((_k)->v), &(_k)->k_i)

#define for_each_btree_node(_trans, _iter, _btree_id, _start,			\
			    _depth, _flags, _b, _do)				\
({										\
	bch2_trans_begin((_trans));						\
										\
	CLASS(btree_node_iter, _iter)((_trans), (_btree_id), _start,		\
				      0, _depth, _flags);			\
	struct btree *_b;							\
	int _ret3 = 0;								\
	do {									\
		u32 _restart_count = bch2_trans_begin((_trans));		\
										\
		_b = bch2_btree_iter_peek_node(&(_iter));			\
		_ret3 = PTR_ERR_OR_ZERO(_b);					\
		if (_ret3)							\
			continue; /* may be restart; re-evaluated below */	\
										\
		if (!_b)							\
			break;							\
										\
		_ret3 = (_do);							\
		if (_ret3 == -BCH_ERR_fc_continue)				\
			_ret3 = 0;						\
		if (_ret3 == -BCH_ERR_fc_break) {				\
			_ret3 = 0;						\
			break;							\
		}								\
		if (_ret3)							\
			continue;						\
										\
		bch2_trans_verify_not_restarted((_trans), _restart_count);	\
										\
		if (bpos_eq((_b)->key.k.p, SPOS_MAX))				\
			break;							\
										\
		bch2_btree_iter_set_pos(&(_iter), bpos_successor((_b)->key.k.p));\
	} while (bch2_err_matches(_ret3, BCH_ERR_transaction_restart) ||	\
		 !_ret3);							\
										\
	_ret3;									\
})

static inline struct bkey_s_c bch2_btree_iter_peek_prev_type(struct btree_iter *iter,
							     enum btree_iter_update_trigger_flags flags)
{
	/* reverse slots aren't defined for extents: peek_slot() is the slot from @pos on */
	BUG_ON((flags & BTREE_ITER_slots) && (iter->flags & BTREE_ITER_is_extents));

	return  flags & BTREE_ITER_slots      ? bch2_btree_iter_peek_slot(iter) :
						bch2_btree_iter_peek_prev(iter);
}

/*
 * Range iteration in either direction: with BTREE_ITER_prev, iteration runs
 * from the iterator's position down to @end (now the inclusive *lower*
 * bound) - the mirror image of the forward case. BTREE_ITER_prev, like
 * BTREE_ITER_slots, is consumed entirely here in the dispatch helpers; the
 * core iterator never sees it.
 */
static inline struct bkey_s_c bch2_btree_iter_peek_max_type(struct btree_iter *iter,
							    struct bpos end,
							    enum btree_iter_update_trigger_flags flags)
{
	if (!(flags & (BTREE_ITER_slots|BTREE_ITER_prev)))
		return bch2_btree_iter_peek_max(iter, &end);

	if (flags & BTREE_ITER_prev) {
		if (!(flags & BTREE_ITER_slots))
			return bch2_btree_iter_peek_prev_min(iter, end);

		if (bkey_lt(iter->pos, end))
			return bkey_s_c_null;

		return bch2_btree_iter_peek_prev_type(iter, flags);
	}

	/*
	 * Trimming, a slot is cut to the window, and an extents window ends where
	 * @end starts: the slot there is past it.
	 */
	if (bch2_btree_iter_trims(iter)) {
		if (bkey_ge(iter->pos, end))
			return bkey_s_c_null;

		return bch2_btree_iter_peek_slot_max(iter, end);
	}

	if (bkey_gt(iter->pos, end))
		return bkey_s_c_null;

	return bch2_btree_iter_peek_slot(iter);
}

/* Advance in iteration order: forwards, or backwards with BTREE_ITER_prev: */
static inline bool bch2_btree_iter_advance_type(struct btree_iter *iter,
						enum btree_iter_update_trigger_flags flags)
{
	return flags & BTREE_ITER_prev
		? bch2_btree_iter_rewind(iter)
		: bch2_btree_iter_advance(iter);
}

static inline int btree_trans_too_many_iters(struct btree_trans *trans)
{
	if (bitmap_weight(trans->paths_allocated, trans->nr_paths) > BTREE_ITER_NORMAL_LIMIT - 8)
		return __bch2_btree_trans_too_many_iters(trans);

	return 0;
}

/*
 * Loop-form, so that __cleanup/CLASS attributes on resources allocated inside
 * _do fire their cleanup on each restart iteration. Callers that need break/
 * continue inside _do to refer to an outer loop must open-code their own
 * restart loop instead of using lockrestart_do (see for_each_btree_key_*).
 */
#define lockrestart_do(_trans, _do)					\
({									\
	int _ret2;							\
	do {								\
		u32 _restart_count = bch2_trans_begin(_trans);		\
		_ret2 = (_do);						\
									\
		if (!_ret2)						\
			bch2_trans_verify_not_restarted(_trans, _restart_count);\
	} while (bch2_err_matches(_ret2, BCH_ERR_transaction_restart));	\
	_ret2;								\
})

#define trans_may_drop_updates_class_init(_trans)				\
({										\
	struct trans_may_drop_updates t = {					\
		.trans	= _trans,						\
		.old	= (_trans)->begin_may_drop_updates,			\
	};									\
	(_trans)->begin_may_drop_updates = true;				\
	t;									\
})

DEFINE_CLASS(trans_may_drop_updates, struct trans_may_drop_updates,
	     _T.trans->begin_may_drop_updates = _T.old,
	     trans_may_drop_updates_class_init(trans),
	     struct btree_trans *trans)

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
	CLASS(trans_may_drop_updates, _may_drop)(_trans);		\
									\
	while (bch2_err_matches(_ret2 = (_do), BCH_ERR_transaction_restart))\
		_restart_count = bch2_trans_begin(_trans);		\
									\
	if (!_ret2)							\
		bch2_trans_verify_not_restarted(_trans, _restart_count);\
									\
	_ret2 ?: trans_was_restarted(_trans, _orig_restart_count);	\
})

#define for_each_btree_key_max_continue(_trans, _iter,			\
					 _end, _flags, _k, _do)		\
({									\
	struct bkey_s_c _k;						\
	int _ret3 = 0;							\
									\
	do {								\
		u32 _restart_count = bch2_trans_begin(_trans);		\
		_ret3 = 0;						\
									\
		(_k) = bch2_btree_iter_peek_max_type(&(_iter),		\
						_end, (_flags));	\
		if (!(_k).k)						\
			break;						\
									\
		_ret3 = bkey_err(_k) ?: (_do);				\
		if (_ret3 == -BCH_ERR_fc_continue)			\
			_ret3 = 0;					\
		if (_ret3 == -BCH_ERR_fc_break) {			\
			_ret3 = 0;					\
			break;						\
		}							\
		if (!_ret3)						\
			bch2_trans_verify_not_restarted(_trans, _restart_count);\
	} while (bch2_err_matches(_ret3, BCH_ERR_transaction_restart) ||\
		 (!_ret3 && bch2_btree_iter_advance_type(&(_iter), (_flags))));\
									\
	_ret3;								\
})

#define for_each_btree_key_continue(_trans, _iter, _flags, _k, _do)	\
	for_each_btree_key_max_continue(_trans, _iter, SPOS_MAX, _flags, _k, _do)

#define for_each_btree_key_max(_trans, _iter, _btree_id,			\
				_start, _end, _flags, _k, _do)			\
({										\
	bch2_trans_begin(trans);						\
										\
	CLASS(btree_iter, _iter)((_trans), (_btree_id), (_start), (_flags));	\
	for_each_btree_key_max_continue(_trans, _iter, _end, _flags, _k, _do);	\
})

#define for_each_btree_key(_trans, _iter, _btree_id, _start, _flags, _k, _do)	\
	for_each_btree_key_max(_trans, _iter, _btree_id, _start, SPOS_MAX, _flags, _k, _do)

#define for_each_btree_key_reverse(_trans, _iter, _btree_id,			\
				   _start, _flags, _k, _do)			\
	for_each_btree_key_max(_trans, _iter, _btree_id, _start, POS_MIN,	\
			       (_flags)|BTREE_ITER_prev, _k, _do)

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

#define for_btree_root_key_at_level(_trans, _iter, _btree_id, _level, _k, _do)		\
	lockrestart_do(trans, ({							\
		CLASS(btree_iter_uninit, iter)(trans);					\
		struct bkey_s_c _k;							\
		bkey_err(k = bch2_btree_iter_peek_root(_trans, &_iter,			\
						       _btree_id, level)) ?:		\
		(k.k ? _do : 0);							\
	}))

#define for_each_btree_key_max_norestart(_trans, _iter, _btree_id,			\
			   _start, _end, _flags, _k, _ret)				\
	for (CLASS(btree_iter, _iter)((_trans), (_btree_id), (_start), (_flags));	\
	     (_k) = bch2_btree_iter_peek_max_type(&(_iter), _end, _flags),		\
	     !((_ret) = bkey_err(_k)) && (_k).k;					\
	     bch2_btree_iter_advance_type(&(_iter), (_flags)))

#define for_each_btree_key_norestart(_trans, _iter, _btree_id,				\
				     _start, _flags, _k, _ret)				\
	for_each_btree_key_max_norestart(_trans, _iter, _btree_id, _start,		\
					  SPOS_MAX, _flags, _k, _ret)

#define for_each_btree_key_max_continue_norestart(_iter, _end, _flags, _k, _ret)	\
	for (;										\
	     (_k) = bch2_btree_iter_peek_max_type(&(_iter), _end, _flags),		\
	     !((_ret) = bkey_err(_k)) && (_k).k;					\
	     bch2_btree_iter_advance_type(&(_iter), (_flags)))

#define for_each_btree_key_continue_norestart(_iter, _flags, _k, _ret)			\
	for_each_btree_key_max_continue_norestart(_iter, SPOS_MAX, _flags, _k, _ret)

#define for_each_btree_key_reverse_norestart(_trans, _iter, _btree_id,			\
					     _start, _flags, _k, _ret)			\
	for_each_btree_key_max_norestart(_trans, _iter, _btree_id, _start,		\
					 POS_MIN, (_flags)|BTREE_ITER_prev, _k, _ret)

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
	gfp_t _gfp = GFP_NOWAIT;					\
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
	gfp_t _gfp = GFP_NOWAIT;					\
	typeof(_do) _p = _do;						\
									\
	_ret = 0;							\
	if (unlikely(!_p)) {						\
		_gfp = GFP_KERNEL;					\
		_ret = drop_locks_do(_trans, ((_p = _do), 0));		\
	}								\
	_p;								\
})

#define allocate_dropping_locks_errcode_norelock(_trans, _do)		\
({									\
	gfp_t _gfp = GFP_NOWAIT;					\
	int _ret = _do;							\
									\
	if (bch2_err_matches(_ret, ENOMEM)) {				\
		bch2_trans_unlock(_trans);				\
		_gfp = GFP_KERNEL;					\
		_ret = _do;						\
	}								\
	_ret;								\
})

#define allocate_dropping_locks_norelock(_trans, _do)			\
({									\
	gfp_t _gfp = GFP_NOWAIT;					\
	typeof(_do) _p = _do;						\
	if (unlikely(!_p)) {						\
		bch2_trans_unlock(_trans);				\
		_gfp = GFP_KERNEL;					\
		_p = _do;						\
	}								\
	_p;								\
})

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

#endif /* _BCACHEFS_BTREE_ITER_H */
