/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_DISK_ACCOUNTING_H
#define _BCACHEFS_DISK_ACCOUNTING_H

#include "btree/update.h"
#include "sb/members.h"
#include "data_structures/eytzinger.h"

#include "alloc/accounting_mem_gen.h"

static inline void bch2_u64s_neg(u64 *v, unsigned nr)
{
	for (unsigned i = 0; i < nr; i++)
		v[i] = -v[i];
}

static inline unsigned bch2_accounting_counters(const struct bkey *k)
{
	return bkey_val_u64s(k) - offsetof(struct bch_accounting, d) / sizeof(u64);
}

static inline void bch2_accounting_neg(struct bkey_s_accounting a)
{
	bch2_u64s_neg(a.v->d, bch2_accounting_counters(a.k));
}

static inline struct bversion journal_pos_to_bversion(struct journal_res *res, unsigned offset)
{
	EBUG_ON(!res->ref);

	return (struct bversion) {
		.hi = res->seq >> 32,
		.lo = (res->seq << 32) | (res->offset + offset),
	};
}

static inline int bch2_accounting_trans_commit_hook(struct btree_trans *trans,
						    struct bkey_i_accounting *a,
						    unsigned commit_flags)
{
	u64 *base = (u64 *) btree_trans_subbuf_base(trans, &trans->accounting);
	a->k.bversion = journal_pos_to_bversion(&trans->journal_res, (u64 *) a - base);

	EBUG_ON(bversion_zero(a->k.bversion));

	return likely(!(commit_flags & BCH_TRANS_COMMIT_skip_accounting_apply))
		? bch2_accounting_mem_add(trans, bkey_i_to_s_c(&a->k_i))
		: 0;
}

static inline void bch2_accounting_trans_commit_revert(struct btree_trans *trans,
						       struct bkey_i_accounting *a_i,
						       unsigned commit_flags)
{
	if (likely(!(commit_flags & BCH_TRANS_COMMIT_skip_accounting_apply))) {
		struct bkey_s_accounting a = accounting_i_to_s(a_i);

		bch2_accounting_neg(a);
		bch2_accounting_mem_add(trans, bkey_i_to_s_c(&a_i->k_i));
		bch2_accounting_neg(a);
	}
}

#endif /* _BCACHEFS_DISK_ACCOUNTING_H */
