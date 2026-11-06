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

static inline bool bch2_accounting_key_is_zero(struct bkey_s_c_accounting a)
{
	for (unsigned i = 0;  i < bch2_accounting_counters(a.k); i++)
		if (a.v->d[i])
			return false;
	return true;
}

static inline void bch2_accounting_accumulate(struct bkey_i_accounting *dst,
					      struct bkey_s_c_accounting src)
{
	for (unsigned i = 0;
	     i < min(bch2_accounting_counters(&dst->k),
		     bch2_accounting_counters(src.k));
	     i++)
		dst->v.d[i] += src.v->d[i];

	if (bversion_cmp(dst->k.bversion, src.k->bversion) < 0)
		dst->k.bversion = src.k->bversion;
}

static inline void bch2_accounting_accumulate_maybe_kill(struct bch_fs *c,
							 struct bkey_i_accounting *dst,
							 struct bkey_s_c_accounting src)
{
	bch2_accounting_accumulate(dst, src);

	if (bch2_accounting_key_is_zero(accounting_i_to_s_c(dst)))
		__bch2_accounting_maybe_kill(c, dst->k.p);
}

static inline void bpos_to_disk_accounting_pos(struct disk_accounting_pos *acc, struct bpos p)
{
	BUILD_BUG_ON(sizeof(*acc) != sizeof(p));

#if __BYTE_ORDER__ == __ORDER_BIG_ENDIAN__
	acc->_pad = p;
#else
	memcpy_swab(acc, &p, sizeof(p));
#endif
}

#define disk_accounting_key_init(_k, _type, ...)			\
do {									\
	memset(&(_k), 0, sizeof(_k));					\
	(_k).type	= BCH_DISK_ACCOUNTING_##_type;			\
	(_k)._type	= (bch_acct_##_type) { __VA_ARGS__ };		\
} while (0)

#define bch2_disk_accounting_mod2(_trans, _gc, _v, ...)			\
({									\
	struct disk_accounting_pos pos;					\
	disk_accounting_key_init(pos, __VA_ARGS__);			\
	bch2_disk_accounting_mod(trans, &pos, _v, ARRAY_SIZE(_v), _gc);	\
})

static inline bool bch2_accounting_is_mem(struct disk_accounting_pos *acc)
{
	return acc->type < BCH_DISK_ACCOUNTING_TYPE_NR &&
		acc->type != BCH_DISK_ACCOUNTING_inum;
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
