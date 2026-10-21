// SPDX-License-Identifier: GPL-2.0
#include "bcachefs.h"

#include "alloc/backpointers.h"

#include "btree/bbpos.h"
#include "btree/write_buffer.h"

#include "init/progress.h"

static int bkey_i_backpointer_cmp(const void *_l, const void *_r)
{
	const struct bkey_i_backpointer *l = _l;
	const struct bkey_i_backpointer *r = _r;
	struct bbpos l_pos = BBPOS(l->v.btree_id, l->v.pos);
	struct bbpos r_pos = BBPOS(r->v.btree_id, r->v.pos);

	/* Sort in reverse order, we'll be iterating in reverse order */
	return -bbpos_cmp(l_pos, r_pos);
}

struct bkey_s_c_backpointer bch2_bp_scan_iter_peek(struct btree_trans *trans,
						   struct bp_scan_iter *iter, struct bpos end,
						   struct wb_maybe_flush *last_flushed)
{
	if (iter->nr_flushes != last_flushed->nr_flushes) {
		if (iter->bps.nr) {
			struct bkey_i_backpointer *prev = &darray_last(iter->bps);

			CLASS(btree_iter, bp_iter)(trans, iter->btree, prev->k.p, 0);
			struct bkey_s_c k;
			int ret = bkey_err(k = bch2_btree_iter_peek_slot(&bp_iter));
			if (bkey_err(k))
				return (struct bkey_s_c_backpointer) { .k = ERR_PTR(ret) };

			if (k.k->type == KEY_TYPE_backpointer)
				bkey_reassemble(&prev->k_i, k);
			else
				--iter->bps.nr;
		}

		iter->nr_flushes = last_flushed->nr_flushes;
	}

	if (!iter->bps.nr) {
		size_t limit = (system_totalram_bytes() / 16) /
			sizeof(struct bkey_i_backpointer);

		u32 restart_count = trans->restart_count;

		int ret = for_each_btree_key_max(trans, bp_iter, iter->btree,
					   iter->pos, end, BTREE_ITER_prefetch, k, ({
			if (k.k->type != KEY_TYPE_backpointer)
				continue;

			/* XXX: this is a really big allocation, we should drop
			 * srcu lock */

			struct bkey_i_backpointer bp;
			bkey_reassemble(&bp.k_i, k);
			if (iter->bps.nr > limit ||
			    darray_push_gfp(&iter->bps, bp, GFP_KERNEL|__GFP_NOWARN))
				break;

			iter->pos = bpos_nosnap_successor(k.k->p);
			(iter->progress
			 ? bch2_progress_update_iter(trans, iter->progress, &bp_iter)
			 : 0);
		}));

		if (ret)
			return ((struct bkey_s_c_backpointer) { .k = ERR_PTR(ret) });

		if (!iter->bps.nr)
			return (struct bkey_s_c_backpointer) {};

		bch2_trans_unlock_long(trans);
		darray_sort(iter->bps, bkey_i_backpointer_cmp);
		bch2_trans_begin(trans);
		trans->restart_count = restart_count;
	}

	return backpointer_i_to_s_c(&darray_last(iter->bps));
}

/* For Rust: a peek can return NULL or an error pointer, which Rust's typed keys can't be */
struct bkey_s_c rust_bp_scan_iter_peek(struct btree_trans *trans, struct bp_scan_iter *iter,
				       struct bpos end, struct wb_maybe_flush *last_flushed)
{
	return bch2_bp_scan_iter_peek(trans, iter, end, last_flushed).s_c;
}
