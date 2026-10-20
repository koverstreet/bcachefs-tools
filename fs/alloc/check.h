/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_ALLOC_CHECK_H
#define _BCACHEFS_ALLOC_CHECK_H

#include "init/error.h"

struct wb_maybe_flush;

/* Alloc fsck is check.rs's; the freespace key check is still check.c's */
#include "alloc/check_gen.h"

#define need_discard_or_freespace_err(...)		\
	fsck_err_wrap(bch2_need_discard_or_freespace_err(__VA_ARGS__))

#define need_discard_or_freespace_err_on(cond, ...)		\
	(unlikely(cond) ?  need_discard_or_freespace_err(__VA_ARGS__) : false)

static inline int bch2_check_freespace_key_async(struct btree_trans *trans,
						 struct btree_iter *iter,
						 u8 *gen, u64 *journal_seq_empty)
{
	return __bch2_check_freespace_key(trans, iter, gen, journal_seq_empty, FSCK_ERR_NO_LOG, NULL);
}

#endif /* _BCACHEFS_ALLOC_CHECK_H */
