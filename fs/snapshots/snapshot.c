// SPDX-License-Identifier: GPL-2.0

#include "bcachefs.h"

#include "btree/iter.h"

#include "snapshots/snapshot.h"

/*
 * Key snapshot overwrite checks: C until their callers, in btree/update.c,
 * are Rust - the Rust version is snapshot::overwrites().
 */

int __bch2_get_snapshot_overwrites(struct btree_trans *trans,
				   enum btree_id btree, struct bpos pos,
				   snapshot_id_list *s)
{
	struct bch_fs *c = trans->c;
	struct bkey_s_c k;
	int ret = 0;

	for_each_btree_key_reverse_norestart(trans, iter, btree, bpos_predecessor(pos),
					     BTREE_ITER_all_snapshots, k, ret) {
		if (!bkey_eq(k.k->p, pos))
			break;

		if (!bch2_snapshot_is_ancestor(trans, k.k->p.snapshot, pos.snapshot) ||
		    snapshot_list_has_ancestor(trans, s, k.k->p.snapshot))
			continue;

		try(snapshot_list_add(c, s, k.k->p.snapshot));
	}

	return ret;
}

int __bch2_key_has_snapshot_overwrites(struct btree_trans *trans,
				       enum btree_id id,
				       struct bpos pos)
{
	struct bkey_s_c k;
	int ret;

	for_each_btree_key_reverse_norestart(trans, iter, id, bpos_predecessor(pos),
					     BTREE_ITER_not_extents|
					     BTREE_ITER_all_snapshots,
					     k, ret) {
		if (!bkey_eq(pos, k.k->p))
			break;

		if (bch2_snapshot_is_ancestor(trans, k.k->p.snapshot, pos.snapshot))
			return 1;
	}

	return ret;
}
