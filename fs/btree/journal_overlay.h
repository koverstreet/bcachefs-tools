/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_BTREE_JOURNAL_ITER_H
#define _BCACHEFS_BTREE_JOURNAL_ITER_H

#include "btree/bkey.h"

#include "btree/journal_overlay_defs_gen.h"

static inline u32 journal_entry_radix_idx(struct bch_fs *c, u64 seq)
{
	return seq - c->journal_entries_base_seq;
}

static inline struct bkey_i *journal_key_k(struct bch_fs *c,
					   const struct journal_key *k)
{
	if (k->allocated)
		return k->allocated_k;

	struct journal_replay *i = *genradix_ptr(&c->journal_entries, k->journal_seq_offset);

	return (struct bkey_i *) (i->j._data + k->journal_offset);
}

static inline int __journal_key_btree_cmp(enum btree_id	l_btree_id,
					  unsigned	l_level,
					  const struct journal_key *r)
{
	return -cmp_int(l_level,	r->level) ?:
		cmp_int(l_btree_id,	r->btree_id);
}

static inline int __journal_key_cmp(struct bch_fs *c,
				    enum btree_id	l_btree_id,
				    unsigned		l_level,
				    struct bpos	l_pos,
				    const struct journal_key *r)
{
	return __journal_key_btree_cmp(l_btree_id, l_level, r) ?:
		bpos_cmp(l_pos, journal_key_k(c, r)->k.p);
}

static inline int journal_key_cmp(struct bch_fs *c,
				  const struct journal_key *l, const struct journal_key *r)
{
	return __journal_key_cmp(c, l->btree_id, l->level,
				 journal_key_k(c, l)->k.p, r);
}

static inline void bch2_journal_keys_put_initial(struct bch_fs *c)
{
	if (c->journal_keys.initial_ref_held)
		bch2_journal_keys_put(c);
	c->journal_keys.initial_ref_held = false;
}

#endif /* _BCACHEFS_BTREE_JOURNAL_ITER_H */
