// SPDX-License-Identifier: GPL-2.0
#include "bcachefs.h"

#include "journal/init.h"
#include "journal/journal.h"
#include "journal/read.h"
#include "journal/reclaim.h"
#include "journal/seq_blacklist.h"

#include "alloc/foreground.h"
#include "alloc/replicas.h"
#include "btree/update.h"
#include "init/error.h"

/* allocate journal on a device: */

static int bch2_set_nr_journal_buckets_iter(struct bch_dev *ca, unsigned nr,
					    bool new_fs, enum bch_watermark watermark,
					    struct closure *cl)
{
	struct bch_fs *c = ca->fs;
	unsigned i, nr_got = 0, nr_have = bch2_journal_buckets_nr(&ca->journal), nr_want = nr - nr_have;
	int ret = 0;

	BUG_ON(nr <= nr_have);

	u64 *bu __free(kfree)			= kcalloc(nr_want, sizeof(*bu), GFP_KERNEL);
	struct open_bucket **ob __free(kfree)	= kcalloc(nr_want, sizeof(*ob), GFP_KERNEL);
	if (!bu || !ob)
		return bch_err_throw(c, ENOMEM_set_nr_journal_buckets);

	for (nr_got = 0; nr_got < nr_want; nr_got++) {
		CLASS(btree_trans, trans)(c);

		ret = lockrestart_do(trans, ({
			struct alloc_request *req __free(alloc_request_put) =
				alloc_request_get(trans,
						  0, false, NULL, 1, 0, watermark, 0,
						  !nr_got ? cl : 0);
			int ret2 = PTR_ERR_OR_ZERO(req) ?:
			(req->ca = ca,
			 req->data_type = BCH_DATA_journal,
			 PTR_ERR_OR_ZERO(ob[nr_got] = bch2_bucket_alloc_trans(trans, req)));

			if (bch2_err_matches(ret2, BCH_ERR_operation_blocked))
				ret2 = bch2_wait_on_allocator(trans, req, ret2, cl) ?:
					btree_trans_restart(trans, BCH_ERR_transaction_restart_nested);

			ret2;
		}));
		if (ret)
			break;

		ret = bch2_trans_mark_metadata_bucket(trans, ca,
					ob[nr_got]->bucket, BCH_DATA_journal,
					ca->mi.bucket_size, BTREE_TRIGGER_transactional);
		if (ret) {
			bch2_open_bucket_put(c, ob[nr_got]);
			bch_err_msg(c, ret, "marking new journal buckets");
			break;
		}

		bu[nr_got] = ob[nr_got]->bucket;
	}

	if (!nr_got)
		return ret;

	/* Don't return an error if we successfully allocated some buckets: */
	ret = 0;

	scoped_guard(journal_block, &c->journal)
		ret = bch2_journal_buckets_add(ca, bu, nr_got);

	if (ret) {
		CLASS(btree_trans, trans)(c);
		for (i = 0; i < nr_got; i++)
			bch2_trans_mark_metadata_bucket(trans, ca,
						bu[i], BCH_DATA_free, 0,
						BTREE_TRIGGER_transactional);
	}

	for (i = 0; i < nr_got; i++)
		bch2_open_bucket_put(c, ob[i]);

	return ret;
}

static int bch2_set_nr_journal_buckets_loop(struct bch_fs *c, struct bch_dev *ca,
					    unsigned nr, bool new_fs)
{
	struct journal_device *ja = &ca->journal;
	enum bch_watermark watermark = new_fs
		? BCH_WATERMARK_btree
		: BCH_WATERMARK_normal;
	int ret = 0;

	CLASS(closure_stack, cl)();

	/* don't handle reducing nr of buckets yet: */
	if (nr < bch2_journal_buckets_nr(ja))
		return 0;

	while (!ret && bch2_journal_buckets_nr(ja) < nr) {

		/*
		 * note: journal buckets aren't really counted as _sectors_ used yet, so
		 * we don't need the disk reservation to avoid the BUG_ON() in buckets.c
		 * when space used goes up without a reservation - but we do need the
		 * reservation to ensure we'll actually be able to allocate:
		 *
		 * XXX: that's not right, disk reservations only ensure a
		 * filesystem-wide allocation will succeed, this is a device
		 * specific allocation - we can hang here:
		 *
		 * A device outside the capacity (durability 0) takes nothing
		 * anyone reserved, so there's nothing to reserve against - and
		 * when initializing a new filesystem, the devices after it
		 * don't have their free space counted yet.
		 */
		CLASS(disk_reservation, res)(c);
		if (!new_fs && dev_has_capacity(ca))
			try(bch2_disk_reservation_add(c, &res.r,
						bucket_to_sector(ca, nr - bch2_journal_buckets_nr(ja)), 1, 0));

		ret = bch2_set_nr_journal_buckets_iter(ca, nr, new_fs, watermark, &cl);
	}

	return ret;
}

/*
 * Allocate more journal space at runtime - not currently making use if it, but
 * the code works:
 */
int bch2_set_nr_journal_buckets(struct bch_fs *c, struct bch_dev *ca,
				unsigned nr)
{
	guard(rwsem_write)(&c->state_lock);

	if (READ_ONCE(ca->removing))
		return bch_err_throw(c, device_has_been_removed);

	int ret = bch2_set_nr_journal_buckets_loop(c, ca, nr, false);
	bch_err_fn(c, ret);
	return ret;
}

int bch2_dev_journal_alloc(struct bch_dev *ca, bool new_fs)
{
	struct bch_fs *c = ca->fs;

	if (!(ca->mi.data_allowed & BIT(BCH_DATA_journal)))
		return 0;

	if (c->sb.features & BIT_ULL(BCH_FEATURE_small_image)) {
		bch_err(c, "cannot allocate journal, filesystem is an unresized image file");
		return bch_err_throw(c, erofs_filesystem_full);
	}

	unsigned nr;
	int ret;

	if (dynamic_fault("bcachefs:add:journal_alloc")) {
		ret = bch_err_throw(c, ENOMEM_set_nr_journal_buckets);
		goto err;
	}

	/* 1/128th of the device by default: */
	nr = ca->mi.nbuckets >> 7;

	/* but no more than a quarter of RAM: */
	nr = clamp_t(unsigned, nr,
		     BCH_JOURNAL_BUCKETS_MIN,
		     div64_u64(system_totalram_bytes() / 4, bucket_bytes(ca)));

	ret = bch2_set_nr_journal_buckets_loop(c, ca, nr, new_fs);
err:
	bch_err_fn_dev(ca, ret);
	return ret;
}

int bch2_fs_journal_alloc(struct bch_fs *c)
{
	for_each_online_member(c, ca, BCH_DEV_READ_REF_fs_journal_alloc) {
		if (bch2_journal_buckets_nr(&ca->journal))
			continue;

		int ret = bch2_dev_journal_alloc(ca, true);
		if (ret) {
			enumerated_ref_put(&ca->io_ref[READ],
					   BCH_DEV_READ_REF_fs_journal_alloc);
			return ret;
		}
	}

	return 0;
}

/* startup/shutdown: */

static bool bch2_journal_writing_to_device(struct journal *j, unsigned dev_idx)
{
	struct bch_fs *c = container_of(j, struct bch_fs, journal);

	guard(spinlock)(&j->lock);

	struct journal_buf *buf;
	u64 seq;
	fifo_for_each_entry_ptr(buf, &j->in_flight, seq)
		if (bch2_bkey_has_device_c(c, bkey_i_to_s_c(&buf->key), dev_idx))
			return true;

	return false;
}

void bch2_dev_journal_stop(struct journal *j, struct bch_dev *ca)
{
	closure_wait_event(&j->async_wait, !bch2_journal_writing_to_device(j, ca->dev_idx));
	cancel_work_sync(&ca->journal.discard);
}

void bch2_fs_journal_stop(struct journal *j)
{
	if (!test_bit(JOURNAL_running, &j->flags))
		return;

	bch2_journal_reclaim_stop(j);

#ifdef CONFIG_BCACHEFS_DEBUG
	j->stop_thread = current;
#endif
	bch2_journal_flush_all_pins(j);

	/*
	 * Always write a new journal entry, to make sure the clock hands are up
	 * to date (and match the superblock)
	 */
	__bch2_journal_meta(j);

	bch2_journal_shutdown_quiesce(j);
	cancel_delayed_work_sync(&j->write_work);

#ifdef CONFIG_BCACHEFS_DEBUG
	j->stop_thread = ERR_PTR(-EROFS);
#endif

	WARN(!bch2_journal_error(j) &&
	     test_bit(JOURNAL_replay_done, &j->flags) &&
	     j->last_empty_seq != journal_cur_seq(j),
	     "journal shutdown error: cur seq %llu but last empty seq %llu",
	     journal_cur_seq(j), j->last_empty_seq);

	/*
	 * All pins have been flushed and the journal quiesced, so every dirty
	 * entry has been reclaimed - dirty_entry_bytes must have drained to 0.
	 * A nonzero remainder means the accounting (journal.c entry-close +/
	 * bch2_journal_update_last_seq_ondisk -) has leaked, which silently
	 * shrinks the in-memory journal-space budget (see mem_limit in
	 * bch2_journal_space_available()).
	 */
	WARN(!bch2_journal_error(j) &&
	     test_bit(JOURNAL_replay_done, &j->flags) &&
	     j->dirty_entry_bytes,
	     "journal shutdown error: %zu dirty entry bytes remaining after flushing all pins",
	     j->dirty_entry_bytes);

	if (!bch2_journal_error(j))
		clear_bit(JOURNAL_running, &j->flags);
}

int bch2_fs_journal_start(struct journal *j, struct journal_start_info info)
{
	struct bch_fs *c = container_of(j, struct bch_fs, journal);
	struct journal_replay *i, **_i;
	struct genradix_iter iter;
	bool had_entries = false;
	int ret = 0;

	/* Don't reuse sequence numbers that are blacklisted */
	info.cur_seq = max(info.cur_seq, bch2_journal_last_blacklisted_seq(c) + 1);

	if (info.cur_seq >= JOURNAL_SEQ_MAX) {
		bch_err(c, "cannot start: journal seq overflow");
		return bch_err_throw(c, EINVAL_journal_seq_overflow);
	}

	/* Clean filesystem? */
	u64 cur_seq	= info.cur_seq;
	u64 last_seq	= info.last_seq ?: info.cur_seq;

	try(bch2_journal_pins_start(j, last_seq, cur_seq));

	j->replay_journal_seq	= last_seq;
	j->replay_journal_seq_end = cur_seq;
	j->last_seq_ondisk	= last_seq;
	j->flushed_seq_ondisk	= cur_seq - 1;
	j->seq_write_started	= cur_seq - 1;
	j->seq_ondisk		= cur_seq - 1;
	j->last_seq		= last_seq;
	if (!j->rewind_seq) {
		j->rewind_seq		= last_seq;
		j->rewind_seq_ondisk	= last_seq;
	}
	atomic64_set(&j->seq, cur_seq - 1);
	atomic64_set(&j->res_seq, cur_seq - 1);

	/*
	 * Align the in_flight FIFO with seq numbering: front == seq_ondisk + 1
	 * == cur_seq initially (empty fifo), so fifo_entry(in_flight, seq)
	 * indexes by seq once entries start pushing.
	 */
	j->in_flight.front = j->in_flight.back = cur_seq;

	u64 seq;
	genradix_for_each(&c->journal_keys.v.entries, iter, _i) {
		i = *_i;

		if (journal_replay_ignore(i))
			continue;

		seq = le64_to_cpu(i->j.seq);
		BUG_ON(seq >= cur_seq);

		if (seq < last_seq)
			continue;

		if (journal_entry_empty(&i->j))
			j->last_empty_seq = le64_to_cpu(i->j.seq);

		struct bch_devs_list seq_devs = !info.clean
			? bch2_journal_replay_devs(c, i)
			: (struct bch_devs_list) {};

		if (seq_devs.nr) {
			CLASS(printbuf, buf)();
			union bch_replicas_padded r;
			bch2_devlist_to_replicas(&r.e, BCH_DATA_journal, seq_devs);
			bch2_replicas_entry_to_text(&buf, &r.e);

			fsck_err_on(!test_bit(JOURNAL_degraded, &j->flags) &&
				    !bch2_replicas_marked(c, &r.e),
				    c, journal_entry_replicas_not_marked,
				    "superblock not marked as containing replicas for journal entry %llu\n%s",
				    le64_to_cpu(i->j.seq), buf.buf);

			if (!bch2_replicas_entry_get(c, &r.e)) {
				guard(percpu_read)(&j->pin_resize_lock);
				bch2_journal_seq_set_devs(j, seq, &seq_devs);
			}
		}

		had_entries = true;
	}

	if (!had_entries)
		j->last_empty_seq = cur_seq - 1; /* to match j->seq */

	scoped_guard(spinlock, &j->lock) {
		j->last_flush_write = jiffies;
		j->reservations.idx = journal_cur_seq(j);
	}

	try(bch2_replicas_gc_reffed(c));
fsck_err:
	return ret;
}

void bch2_journal_set_replay_done(struct journal *j)
{
	/*
	 * journal_space_available must happen before setting JOURNAL_running
	 * JOURNAL_running must happen before JOURNAL_replay_done
	 */
	guard(spinlock)(&j->lock);
	bch2_journal_space_available(j);

	set_bit(JOURNAL_need_flush_write, &j->flags);
	set_bit(JOURNAL_running, &j->flags);
	set_bit(JOURNAL_replay_done, &j->flags);
}

/* init/exit: */

void bch2_dev_journal_exit(struct bch_dev *ca)
{
	struct journal_device *ja = &ca->journal;

	bioset_exit(&ja->bio_set);
}

int bch2_dev_journal_init(struct bch_dev *ca)
{
	struct bch_fs *c = ca->fs;
	struct journal_device *ja = &ca->journal;

	/*
	 * Pool size is just the mempool reserve for forward progress under
	 * memory pressure; JOURNAL_STATE_BUF_NR matches the reservation
	 * fastpath ring (the effective parallel-write ceiling for hot-path
	 * allocations).
	 */
	if (bioset_init(&ja->bio_set, JOURNAL_STATE_BUF_NR,
			offsetof(struct journal_bio, bio),
			BIOSET_NEED_BVECS))
		return bch_err_throw(c, ENOMEM_dev_journal_init);

	return 0;
}

void bch2_fs_journal_exit(struct journal *j)
{
	darray_exit(&j->early_journal_entries);
	darray_exit(&j->rewind_ranges);

	/*
	 * Any bufs still in the in_flight fifo are leftover on error paths
	 * (normal shutdown drains them).
	 */
	{
		struct journal_buf *buf;
		u64 seq;

		fifo_for_each_entry_ptr(buf, &j->in_flight, seq)
			kvfree(buf->data);
	}
	free_fifo(&j->in_flight);

	kvfree(j->free_buf);
	cancel_work_sync(&j->pin_resize_work);
	cancel_work_sync(&j->flush_degraded_work);
}

int bch2_fs_journal_init_rw(struct journal *j)
{
	struct bch_fs *c = container_of(j, struct bch_fs, journal);

	j->free_buf_size = j->buf_size_want = JOURNAL_ENTRY_SIZE_MIN;
	j->free_buf = kvmalloc(j->free_buf_size, GFP_KERNEL);
	if (!j->free_buf)
		return bch_err_throw(c, ENOMEM_journal_buf);

	/*
	 * in_flight holds one entry per seq in (seq_ondisk, cur_seq]. Size is
	 * an upper bound on in-flight journal writes; starts at 256, which is
	 * well above the 16-slot cap of the prior static-array layout. If
	 * workloads saturate this, journal_entry_open() returns ENOMEM and
	 * the existing retry path throttles via completion backpressure.
	 *
	 * Open-coded instead of init_fifo() because bch2_fs_journal_start()
	 * already set front/back to the correct seq-aligned value, and
	 * init_fifo() would unconditionally reset them to 0.
	 */
	j->in_flight.size = 256;
	j->in_flight.mask = 255;
	j->in_flight.data = kvmalloc_array(256, sizeof(*j->in_flight.data),
					   GFP_KERNEL);
	if (!j->in_flight.data)
		return bch_err_throw(c, ENOMEM_journal_buf);

	for (struct journal_buf *buf = j->in_flight.data;
	     buf < j->in_flight.data + j->in_flight.size;
	     buf++)
		buf->wait.list.first = JOURNAL_BUF_NOT_IN_FLIGHT;

	return 0;
}
