/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_BTREE_JOURNAL_ITER_H
#define _BCACHEFS_BTREE_JOURNAL_ITER_H

#include "btree/bkey.h"

#include "btree/journal_overlay_defs_gen.h"

static inline struct bkey_i *journal_key_k(struct bch_fs *c,
					   const struct journal_key *k)
{
	if (k->allocated)
		return k->allocated_k;

	struct journal_replay *i = *genradix_ptr(&c->journal_keys.v.entries, k->journal_seq_offset);

	return (struct bkey_i *) (i->j._data + k->journal_offset);
}

#endif /* _BCACHEFS_BTREE_JOURNAL_ITER_H */
