/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_BTREE_UPDATE_H
#define _BCACHEFS_BTREE_UPDATE_H

#include "btree/iter.h"
#include "journal/journal.h"
#include "sb/io.h"
#include "snapshots/snapshot.h"

#include "btree/update_gen.h"
#include "btree/commit_gen.h"
#include "btree/node/insert_gen.h"

static inline int __must_check
bch2_trans_update_buf(struct btree_trans *trans, struct btree_iter *iter,
		      struct bkey_i *k, unsigned k_buf_u64s,
		      enum btree_iter_update_trigger_flags flags)
{
	return bch2_trans_update_ip(trans, iter, k, k_buf_u64s, flags, _THIS_IP_);
}

static inline int __must_check
bch2_trans_update(struct btree_trans *trans, struct btree_iter *iter,
		  struct bkey_i *k, enum btree_iter_update_trigger_flags flags)
{
	return bch2_trans_update_ip(trans, iter, k, k->k.u64s, flags, _THIS_IP_);
}

static inline void *btree_trans_subbuf_base(struct btree_trans *trans,
					    struct btree_trans_subbuf *buf)
{
	return trans->mem.data + buf->base;
}

static inline void *btree_trans_subbuf_top(struct btree_trans *trans,
					   struct btree_trans_subbuf *buf)
{
	return trans->mem.data + buf->base + buf->u64s;
}

static inline void *
bch2_trans_subbuf_alloc_ip(struct btree_trans *trans,
			   struct btree_trans_subbuf *buf,
			   unsigned u64s, ulong ip)
{
	if (buf->u64s + u64s > buf->size)
		return __bch2_trans_subbuf_alloc(trans, buf, u64s, ip);

	void *p = btree_trans_subbuf_top(trans, buf);
	buf->u64s += u64s;
	return p;
}

static inline struct jset_entry *btree_trans_journal_entries_start(struct btree_trans *trans)
{
	return btree_trans_subbuf_base(trans, &trans->journal_entries);
}

static inline struct jset_entry *btree_trans_journal_entries_top(struct btree_trans *trans)
{
	return btree_trans_subbuf_top(trans, &trans->journal_entries);
}

static inline struct jset_entry *
bch2_trans_jset_entry_alloc_ip(struct btree_trans *trans, unsigned u64s, ulong ip)
{
	return bch2_trans_subbuf_alloc_ip(trans, &trans->journal_entries, u64s, ip);
}

static inline struct jset_entry *
bch2_trans_jset_entry_alloc(struct btree_trans *trans, unsigned u64s)
{
	return bch2_trans_jset_entry_alloc_ip(trans, u64s, _THIS_IP_);
}

#define trans_for_each_update(_trans, _i)				\
	for (struct btree_insert_entry *_i = (_trans)->updates;		\
	     (_i) < (_trans)->updates + (_trans)->nr_updates;		\
	     (_i)++)

/**
 * bch2_trans_commit - insert keys at given iterator positions
 *
 * This is main entry point for btree updates.
 *
 * Return values:
 * -EROFS: filesystem read only
 * -EIO: journal or btree node IO error
 */
static inline int bch2_trans_commit(struct btree_trans *trans,
				    struct disk_reservation *disk_res,
				    u64 *journal_seq,
				    enum bch_trans_commit_flags flags)
{
	trans->disk_res		= disk_res;
	trans->journal_seq	= journal_seq;
	trans->flush		= NULL;

	return __bch2_trans_commit(trans, flags, false);
}

static inline int bch2_trans_commit_lazy(struct btree_trans *trans,
					 struct disk_reservation *disk_res,
					 u64 *journal_seq,
					 unsigned flags)
{
	if (!bch2_trans_has_updates(trans))
		return 0;

	trans->disk_res		= disk_res;
	trans->journal_seq	= journal_seq;
	trans->flush		= NULL;

	return __bch2_trans_commit(trans, flags, true);
}

/*
 * If _do fails, bch2_trans_commit() never runs, and never resets what _do had
 * queued. An error that isn't a restart abandons the attempt: drop them, as
 * the commit would have, so the next bch2_trans_begin() doesn't take them for
 * updates someone forgot to commit. (A restart's are dropped by the restart.)
 */
#define __commit_do(_trans, _disk_res, _journal_seq, _flags, _do)	\
({									\
	int _ret3 = (_do);						\
									\
	if (unlikely(_ret3) &&						\
	    !bch2_err_matches(_ret3, BCH_ERR_transaction_restart))	\
		bch2_trans_reset_updates(_trans);			\
									\
	_ret3 ?: bch2_trans_commit(_trans, (_disk_res),			\
				   (_journal_seq), (_flags));		\
})

#define commit_do(_trans, _disk_res, _journal_seq, _flags, _do)	\
	lockrestart_do(_trans,						\
		__commit_do(_trans, _disk_res, _journal_seq, _flags, _do))

#define nested_commit_do(_trans, _disk_res, _journal_seq, _flags, _do)	\
	nested_lockrestart_do(_trans,					\
		__commit_do(_trans, _disk_res, _journal_seq, _flags, _do))

#endif /* _BCACHEFS_BTREE_UPDATE_H */
