// SPDX-License-Identifier: GPL-2.0

#include "bcachefs.h"

#include "btree/interior.h"
#include "btree/update.h"

#include "data/reconcile/trigger.h"

#include "init/error.h"

struct bkey_s_c reconcile_bp_get_key(struct btree_trans *trans,
				     struct btree_iter *iter,
				     struct bkey_s_c_backpointer bp)
{
	struct bch_fs *c = trans->c;
	int ret = 0;
	CLASS(printbuf, buf)();

	/*
	 * we're still using fsck_err() here, which does a goto, which has
	 * problems with CLASS()
	 */
	CLASS(btree_iter_uninit, iter2)(trans);

	/* don't allow bps to non btree nodes: */
	if (fsck_err_on(!bp.v->level,
			trans, reconcile_bp_to_leaf_node_key,
			"reconcile bp to leaf node key\n%s",
			(bch2_bkey_val_to_text(&buf, c, bp.s_c), buf.buf))) {
		ret =   bch2_btree_delete(trans, BTREE_ID_reconcile_scan, bp.k->p, 0) ?:
			bch2_trans_commit(trans, NULL, NULL, BCH_TRANS_COMMIT_no_enospc);
		return ret ? bkey_s_c_err(ret) : bkey_s_c_null;
	}

	bch2_trans_node_iter_init(trans, iter, bp.v->btree_id, bp.v->pos, 0, bp.v->level, 0);

	/* walk down a level - we need to have the node pointed to locked, not
	 * the parent node, for synchronization with btree_node_update_key when
	 * the node isn't yet written */

	bch2_trans_node_iter_init(trans, &iter2, bp.v->btree_id, bp.v->pos, 0, bp.v->level - 1, 0);
	struct btree *b = bch2_btree_iter_peek_node(&iter2);
	if (IS_ERR(b))
		return bkey_s_c_err(PTR_ERR(b));

	struct bkey_s_c k = bkey_s_c_null;
	if (b) {
		if (btree_node_will_make_reachable(b))
			return bkey_s_c_null;

		k = bkey_i_to_s_c(&b->key);
		if (bpos_eq(bp.k->p, bch2_bkey_get_reconcile_bp_pos(c, k)))
			return k;
	}

	prt_printf(&buf, "reconcile backpointer to missing/incorrect btree ptr\n");
	bch2_bkey_val_to_text(&buf, c, bp.s_c);
	prt_newline(&buf);
	if (k.k)
		bch2_bkey_val_to_text(&buf, c, k);
	else
		prt_str(&buf, "(no key)");

	if (b) {
		prt_newline(&buf);
		bch2_bkey_val_to_text(&buf, c, bkey_i_to_s_c(&b->key));
	}

	if (fsck_err(trans, reconcile_bp_to_missing_btree_ptr, "%s", buf.buf))
		ret =   bch2_btree_delete(trans, BTREE_ID_reconcile_scan, bp.k->p, 0) ?:
			bch2_trans_commit(trans, NULL, NULL, BCH_TRANS_COMMIT_no_enospc);
fsck_err:
	return ret ? bkey_s_c_err(ret) : bkey_s_c_null;
}
