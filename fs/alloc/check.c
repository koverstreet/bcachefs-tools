// SPDX-License-Identifier: GPL-2.0
/*
 * The freespace key check the allocator runs - and fsck, check.rs's
 * check_alloc_info - with its repair from the allocator path, which can't
 * commit there and goes through a work item. The rest of alloc fsck is
 * check.rs's; this converts with the async work.
 */
#include "bcachefs.h"

#include "alloc/background.h"
#include "alloc/check.h"

#include "btree/update.h"
#include "btree/write_buffer.h"

#include "init/error.h"

struct check_freespace_key_async {
	struct work_struct	work;
	struct bch_fs		*c;
	struct bbpos		pos;
};

static int bch2_recheck_freespace_key(struct btree_trans *trans, struct bbpos pos)
{
	CLASS(btree_iter, iter)(trans, pos.btree, pos.pos, 0);
	struct bkey_s_c k = bkey_try(bch2_btree_iter_peek_slot(&iter));

	u8 gen;
	return k.k->type != KEY_TYPE_set
		? __bch2_check_freespace_key(trans, &iter, &gen, NULL, FSCK_ERR_SILENT, NULL)
		: 0;
}

static void check_discard_freespace_key_work(struct work_struct *work)
{
	struct check_freespace_key_async *w =
		container_of(work, struct check_freespace_key_async, work);

	bch2_trans_do(w->c, bch2_recheck_freespace_key(trans, w->pos));
	enumerated_ref_put(&w->c->writes, BCH_WRITE_REF_check_discard_freespace_key);
	kfree(w);
}

static int delete_freespace_key(struct btree_trans *trans,
				struct btree_iter *iter,
				bool async_repair)
{
	struct bch_fs *c = trans->c;

	if (!async_repair) {
		try(bch2_btree_bit_mod_iter(trans, iter, false));
		try(bch2_trans_commit(trans, NULL, NULL, BCH_TRANS_COMMIT_no_enospc));
		return btree_trans_restart(trans, BCH_ERR_transaction_restart_commit);
	} else {
		/*
		 * We can't repair here when called from the allocator path: the
		 * commit will recurse back into the allocator
		 */
		struct check_freespace_key_async *w = kzalloc(sizeof(*w), GFP_KERNEL);
		if (!w)
			return -BCH_ERR_bucket_not_allocatable;

		if (!enumerated_ref_tryget(&c->writes, BCH_WRITE_REF_check_discard_freespace_key)) {
			kfree(w);
			return -BCH_ERR_bucket_not_allocatable;
		}

		INIT_WORK(&w->work, check_discard_freespace_key_work);
		w->c = c;
		w->pos = BBPOS(iter->btree_id, iter->pos);
		queue_work(c->write_ref_wq, &w->work);
		return -BCH_ERR_bucket_not_allocatable;
	}
}

int __bch2_check_freespace_key(struct btree_trans *trans, struct btree_iter *iter, u8 *gen,
			       u64 *journal_seq_empty, enum bch_fsck_flags fsck_flags,
			       struct wb_maybe_flush *last_flushed)
{
	struct bch_fs *c = trans->c;
	CLASS(printbuf, buf)();
	int ret = 0;

	bool async_repair = fsck_flags & FSCK_ERR_NO_LOG;
	fsck_flags |= FSCK_CAN_FIX|FSCK_CAN_IGNORE;

	struct bpos bucket = iter->pos;
	bucket.offset &= ~(~0ULL << 56);
	u64 genbits = iter->pos.offset & (~0ULL << 56);

	/*
	 * async_repair means we're the allocator: committed-only reads, see
	 * bucket_alloc_scan(). fsck's synchronous use keeps normal semantics:
	 */
	CLASS(btree_iter, alloc_iter)(trans, BTREE_ID_alloc, bucket,
						     async_repair
						     ? BTREE_ITER_cached|BTREE_ITER_committed
						     : 0);
	struct bkey_s_c alloc_k = bkey_try(bch2_btree_iter_peek_slot(&alloc_iter));

	if (!bch2_dev_bucket_exists(c, bucket)) {
		if (__fsck_err(trans, fsck_flags,
			       need_discard_freespace_key_to_invalid_dev_bucket,
			       "entry in %s btree for nonexistant dev:bucket %llu:%llu",
			       bch2_btree_id_str(iter->btree_id), bucket.inode, bucket.offset))
			ret = delete_freespace_key(trans, iter, async_repair);
		else
			ret = -BCH_ERR_bucket_not_allocatable;
		goto out;
	}

	struct bch_alloc_v4 a_convert;
	const struct bch_alloc_v4 *a = bch2_alloc_to_v4(alloc_k, &a_convert);

	if (a->data_type != BCH_DATA_free ||
	    genbits != alloc_freespace_genbits(*a)) {
		if (last_flushed)
			try(bch2_btree_write_buffer_maybe_flush(trans, alloc_k, last_flushed));

		if (__fsck_err(trans, fsck_flags,
			       need_discard_freespace_key_bad,
			     "%s\nincorrectly set at %s:%llu:%llu:0 (genbits %llu should be %llu)",
			     (bch2_bkey_val_to_text(&buf, c, alloc_k), buf.buf),
			     bch2_btree_id_str(iter->btree_id),
			     iter->pos.inode,
			     iter->pos.offset,
			     genbits >> 56, alloc_freespace_genbits(*a) >> 56))
			ret = delete_freespace_key(trans, iter, async_repair);
		else
			ret = -BCH_ERR_bucket_not_allocatable;
		goto out;
	}

	*gen = a->generation;
	if (journal_seq_empty)
		*journal_seq_empty = a->journal_seq_empty;
out:
fsck_err:
	bch2_set_btree_iter_dontneed(&alloc_iter);
	return ret;
}
