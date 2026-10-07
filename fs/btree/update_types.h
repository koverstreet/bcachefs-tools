/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_BTREE_UPDATE_TYPES_H
#define _BCACHEFS_BTREE_UPDATE_TYPES_H

#include "enum_kind.h"

struct bch_fs;

struct btree;

#define BCH_TRANS_COMMIT_FLAGS()							\
	x(no_enospc,	"don't check for enospc")					\
	x(no_check_rw,	"don't attempt to take a ref on c->writes")			\
	x(no_journal_res, "don't take a journal reservation, instead "			\
			"pin journal entry referred to by trans->journal_res.seq")	\
	x(no_skip_noops, "don't drop noop updates")					\
	x(journal_reclaim, "operation required for journal reclaim; may return error"	\
			"instead of deadlocking if BCH_WATERMARK_reclaim not specified")\
	x(journal_replay, "in journal replay")						\
	x(skip_accounting_apply, "we're in journal replay - accounting updates have already been applied")

enum __enum_closed __bch_trans_commit_flags {
	/* First bits for bch_watermark: */
	__BCH_TRANS_COMMIT_FLAGS_START = BCH_WATERMARK_BITS,
#define x(n, ...)	__BCH_TRANS_COMMIT_##n,
	BCH_TRANS_COMMIT_FLAGS()
#undef x
};

enum __enum_flags bch_trans_commit_flags {
#define x(n, ...)	BCH_TRANS_COMMIT_##n = BIT(__BCH_TRANS_COMMIT_##n),
	BCH_TRANS_COMMIT_FLAGS()
#undef x
};

#endif /* _BCACHEFS_BTREE_UPDATE_TYPES_H */
