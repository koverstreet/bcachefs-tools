// SPDX-License-Identifier: GPL-2.0

#include "bcachefs.h"

#include "alloc/background.h"
#include "alloc/disk_groups.h"
#include "alloc/foreground.h"
#include "alloc/backpointers.h"
#include "alloc/replicas.h"

#include "btree/bkey_buf.h"
#include "btree/check.h"
#include "btree/interior.h"
#include "btree/read.h"
#include "btree/update.h"
#include "btree/write_buffer.h"

#include "data/compress.h"
#include "data/keylist.h"
#include "data/extents.h"
#include "data/ec/create.h"
#include "data/ec/io.h"
#include "data/ec/trigger.h"
#include "data/move.h"
#include "data/update.h"
#include "data/read.h"
#include "data/reconcile/trigger.h"
#include "data/reflink.h"
#include "data/write.h"

#include "fs/inode.h"

#include "init/damage.h"
#include "init/error.h"
#include "init/recovery.h"

#include "journal/reclaim.h"

#include "sb/counters.h"
#include "sb/io.h"

#include "snapshots/snapshot.h"

#include <linux/ioprio.h>
#include <linux/kthread.h>

void bch2_move_stats_init(struct bch_move_stats *stats, const char *name)
{
	memset(stats, 0, sizeof(*stats));
	stats->data_type = BCH_DATA_user;
	scnprintf(stats->name, sizeof(stats->name), "%s", name);
}

#include "journal/read.h"
#include "btree/journal_overlay.h"

/*
 * Devices with a replica the scrub found bad in what it queued from @start on.
 * A replica that fails its read is retried off another one: the move stats
 * only see the reads that failed on every replica.
 */
static void scrub_journal_bad_replica_devs(struct bch_fs *c, size_t start,
					   struct bch_devs_mask *devs)
{
	guard(mutex)(&c->scrub_journal_repairs_lock);

	for (size_t i = start; i < c->scrub_journal_repairs.nr; i++) {
		scrub_journal_repair *r = &c->scrub_journal_repairs.data[i];
		unsigned ptr_bit = 1;

		bkey_for_each_ptr(bch2_bkey_ptrs_c(bkey_i_to_s_c(&r->k)), ptr) {
			if (r->bad_devs & ptr_bit)
				__set_bit(ptr->dev, devs->d);
			ptr_bit <<= 1;
		}
	}
}

/*
 * A btree node the journal scrub found a pointer to in the window.
 *
 * Extents get their verdict from the read path: a replica that fails is
 * retried off the others, so "uncorrected" already means no replica was good.
 * Node scrub has no retry - each replica is checked on its own - so every
 * replica is checked here, and the verdict made once they've all reported.
 * Some good: the node is rewritten from one once we're rw. None good: a rewind
 * won't help - it only rolls back leaf keys, interior pointers stay (see
 * bch2_journal_keys_sort()), so the node would still be reached. The btree is
 * flagged as having lost data instead, which gets topology repair run before
 * journal replay: found by replay, it would be too late - we're rw by then.
 */
struct scrub_journal_lost_nodes {
	atomic_t		nr;
	DECLARE_BITMAP(btrees, BTREE_ID_NR);
};

struct scrub_journal_node {
	struct closure		cl;
	struct moving_context	*ctxt;
	struct scrub_journal_lost_nodes *lost;
	struct bch_fs		*c;
	enum btree_id		btree;
	unsigned		level;
	unsigned		checked;	/* pointers whose replicas were read */
	atomic_t		bad;		/* ... and found bad */
	struct bkey_buf		key;
};

static void scrub_journal_node_report(void *priv, unsigned dev, bool good)
{
	struct scrub_journal_node *n = priv;

	if (!good)
		atomic_or(bch2_bkey_dev_ptr_bit(n->c, bkey_i_to_s_c(n->key.k), dev), &n->bad);
	closure_put(&n->cl);
}

static CLOSURE_CALLBACK(scrub_journal_node_free)
{
	closure_type(n, struct scrub_journal_node, cl);

	bch2_bkey_buf_exit(&n->key);
	kfree(n);
}

static CLOSURE_CALLBACK(scrub_journal_node_done)
{
	closure_type(n, struct scrub_journal_node, cl);
	struct bch_fs *c = n->c;
	struct bch_move_stats *stats = n->ctxt->stats;
	unsigned bad = atomic_read(&n->bad);

	if (bad && bad == n->checked) {
		atomic_inc(&n->lost->nr);
		set_bit(n->btree, n->lost->btrees);

		/* just the devices, for reporting: uncorrected sectors mean a rewind */
		unsigned ptr_bit = 1;
		bkey_for_each_ptr(bch2_bkey_ptrs_c(bkey_i_to_s_c(n->key.k)), ptr) {
			if (bad & ptr_bit)
				set_bit(ptr->dev, stats->devs_error_uncorrected.d);
			ptr_bit <<= 1;
		}
	} else if (bad) {
		bch2_scrub_journal_queue(c, n->btree, n->level, n->key.k, bad, 0);
	}

	closure_return_with_destructor(cl, scrub_journal_node_free);
}

static int scrub_journal_node(struct moving_context *ctxt,
			      struct scrub_journal_lost_nodes *lost,
			      enum btree_id btree, unsigned level, struct bkey_s_c k)
{
	struct btree_trans *trans = ctxt->trans;
	struct bch_fs *c = trans->c;

	struct scrub_journal_node *n = kzalloc(sizeof(*n), GFP_KERNEL);
	if (!n)
		return bch_err_throw(c, ENOMEM_scrub_journal_node);

	n->ctxt		= ctxt;
	n->lost		= lost;
	n->c		= c;
	n->btree	= btree;
	n->level	= level;
	bch2_bkey_buf_init(&n->key);
	bch2_bkey_buf_reassemble(&n->key, k);
	closure_init(&n->cl, &ctxt->cl);

	int ret = 0;
	unsigned ptr_bit = 1;
	bkey_for_each_ptr(bch2_bkey_ptrs_c(k), ptr) {
		/* same as extents: an absent device tells us nothing */
		if (bch2_dev_idx_is_online(c, ptr->dev)) {
			closure_get(&n->cl);
			int ret2 = bch2_btree_node_scrub_report(trans, btree, level, k, ptr->dev,
								scrub_journal_node_report, n);
			if (!ret2)
				n->checked |= ptr_bit;
			else {
				closure_put(&n->cl);
				ret = ret ?: ret2;
			}
		}
		ptr_bit <<= 1;
	}

	continue_at(&n->cl, scrub_journal_node_done, NULL);
	return ret;
}

int bch2_scrub_journal(struct bch_fs *c, u64 *rewind_seq)
{
	CLASS(darray_u64, flushes)();
	int ret = 0;

	*rewind_seq = 0;

	struct journal_replay **_i;
	struct genradix_iter radix_iter;

	/*
	 * The window is measured back from the newest flush entry, so we need
	 * that before we can decide what falls inside it - genradix_for_each()
	 * runs oldest-first, and the scrub loop below requires @flushes in
	 * ascending seq order, so take two passes rather than reversing.
	 */
	u64 newest_time = 0;
	genradix_for_each(&c->journal_entries, radix_iter, _i) {
		struct journal_replay *i = *_i;

		if (!journal_replay_ignore(i) && !JSET_NO_FLUSH(&i->j))
			newest_time = max(newest_time, (u64) jset_datetime(&i->j));
	}

	genradix_for_each(&c->journal_entries, radix_iter, _i) {
		struct journal_replay *i = *_i;

		if (journal_replay_ignore(i) || JSET_NO_FLUSH(&i->j))
			continue;

		/*
		 * Entries with no datetime entry (t == 0) are always included:
		 * we can't tell how old they are, and excluding them would
		 * silently shrink the scrub window.
		 */
		u64 t = jset_datetime(&i->j);
		if (t && newest_time &&
		    time_before64(t + c->opts.scrub_journal_max_rewind_secs, newest_time))
			continue;

		try(darray_push(&flushes, le64_to_cpu(i->j.seq)));
	}

	event_inc_trace(c, journal_scrub, buf,
			prt_printf(&buf, "%zu flush entries in the window", flushes.nr));

	if (!flushes.nr) {
		bch_info(c, "%s(): No journal flushes found", __func__);
		return 0;
	}

	/* a range is the entries between two flushes */
	bch_info(c, "journal scrub: checking %zu flush ranges",
		 flushes.nr - 1);

	struct bch_move_stats stats;
	bch2_move_stats_init(&stats, "journal_scrub");

	struct moving_context ctxt __cleanup(bch2_moving_ctxt_exit);
	bch2_moving_ctxt_init(&ctxt, c, &stats, (struct write_point_specifier) {}, false);

	struct btree_trans *trans = ctxt.trans;
	struct journal_keys *keys = &c->journal_keys;

	/*
	 * Scrub one flush range at a time, newest first. Within each range,
	 * fire off all reads asynchronously, then flush once to check for
	 * errors. Walk backwards until two ranges in a row read clean; rewind
	 * to the start of the oldest range with data no replica could be read
	 * back for - everything newer is tainted. A bad replica read back from
	 * a good one is repaired from it instead, once we're rw.
	 *
	 * Stop if we've gone further back than scrub_journal_max_rewind_secs seconds.
	 */
	unsigned nr_good = 0;
	struct scrub_journal_lost_nodes lost_nodes = {};

	for (int f = flushes.nr - 1; f > 0; f--) {
		u64 range_start = flushes.data[f - 1];
		u64 range_end   = flushes.data[f];

		event_inc_trace(c, journal_scrub_range, buf,
				prt_printf(&buf, "seq %llu-%llu", range_start + 1, range_end));

		u64 errors_before = atomic64_read(&stats.sectors_error_uncorrected);
		memset(&stats.devs_error_uncorrected, 0, sizeof(stats.devs_error_uncorrected));
		int move_ret = 0;

		size_t queued_before;
		scoped_guard(mutex, &c->scrub_journal_repairs_lock)
			queued_before = c->scrub_journal_repairs.nr;
		unsigned nodes_lost_before = atomic_read(&lost_nodes.nr);

		darray_for_each(*keys, jk) {
			u64 seq = c->journal_entries_base_seq + jk->journal_seq_offset;
			struct bkey_s_c k = bkey_i_to_s_c(journal_key_k(c, jk));


			if (seq <= range_start || seq > range_end)
				continue;

			struct bkey_ptrs_c ptrs = bch2_bkey_ptrs_c(k);
			if (!ptrs.start)
				continue;

			/*
			 * A pointer to a node checks the whole node, bsets
			 * appended since included. New roots are btree_root
			 * entries, not keys, so they're not seen here - they
			 * don't need to be only because read_btree_roots() reads
			 * every root before we run. If that ever changes, they
			 * need checking here too.
			 */
			if (bkey_is_btree_ptr(k.k)) {
				ret = scrub_journal_node(&ctxt, &lost_nodes, jk->btree_id, jk->level, k);
				if (ret)
					move_ret = ret;
				continue;
			}

			struct bch_inode_opts io_opts;
			bch2_inode_opts_get(c, &io_opts, false);

			bkey_for_each_ptr(ptrs, ptr) {
				/*
				 * A device that isn't there tells us nothing
				 * about whether it honoured flush/FUA - don't
				 * let it read as a checksum error and rewind.
				 */
				if (!bch2_dev_idx_is_online(c, ptr->dev))
					continue;

				struct data_update_opts data_opts = {
					.type		= BCH_DATA_UPDATE_scrub_no_repair,
					.read_flags	= BCH_READ_hard_require_read_device,
					.read_dev	= ptr->dev,
				};
				ret = lockrestart_do(trans, ({
					struct bkey_s_c k2;
					u32 restart_count = trans->restart_count;

					CLASS(btree_node_iter, iter)(trans, jk->btree_id, k.k->p, 0, jk->level,
								     BTREE_ITER_all_snapshots);
					int ret2 = bkey_err(k2 = bch2_btree_iter_peek_slot(&iter));

					BUG_ON(!ret2 && !bkey_and_val_eq(k, k2));

					ret2 = ret2 ?:
					bch2_move_extent(&ctxt, NULL, &io_opts, &data_opts, &iter, jk->level, k);

					/*
					 * Suppress trans_was_restarted() check: move_extent
					 * reports read errors by resolving the inode path,
					 * which restarts - and the move is finished either
					 * way. Restoring the count has to happen in here,
					 * not outside: the check that fires is
					 * lockrestart_do()'s own, against the counter it
					 * captured after its bch2_trans_begin().
					 */
					trans->restart_count = restart_count;
					ret2;
				}));

				if (ret)
					move_ret = ret;
			}
		}

		bch2_moving_ctxt_flush_all(&ctxt);

		if (move_ret)
			bch_err(c, "journal scrub: move error %s in flush range seq %llu-%llu",
				bch2_err_str(move_ret),
				range_start + 1, range_end == U64_MAX ? range_start : range_end);

		/* uncorrected: every replica failed, so only a rewind fixes it */
		bool lost = atomic64_read(&stats.sectors_error_uncorrected) != errors_before;
		bool repair;
		scoped_guard(mutex, &c->scrub_journal_repairs_lock)
			repair = c->scrub_journal_repairs.nr != queued_before;
		/* no rewind for these: see struct scrub_journal_node */
		bool range_lost_nodes = atomic_read(&lost_nodes.nr) != nodes_lost_before;

		if (lost || repair || range_lost_nodes) {
			struct bch_devs_mask devs = stats.devs_error_uncorrected;
			scrub_journal_bad_replica_devs(c, queued_before, &devs);

			/* a lost node is counted when recovery reads it */
			if (lost)
				bch2_sb_error_count(c, BCH_FSCK_ERR_device_bad_flush_forced_rewind);
			else if (repair)
				bch2_sb_error_count(c, BCH_FSCK_ERR_device_bad_flush_repaired_from_replica);

			CLASS(printbuf, what)();
			if (lost)
				prt_str(&what, "data with no good replica");
			else if (repair)
				prt_str(&what, "bad replicas, to repair from good ones");
			if (range_lost_nodes)
				prt_printf(&what, "%sbtree nodes with no good replica",
					   what.pos ? ", " : "");

			CLASS(bch_log_msg, msg)(c);
			prt_printf(&msg.m, "journal scrub: %s in flush range seq %llu-%llu, "
				"device(s) not honoring flush/FUA:",
				what.buf,
				range_start + 1, range_end == U64_MAX ? range_start : range_end);

			unsigned i;
			for_each_set_bit(i, devs.d, BCH_SB_MEMBERS_MAX) {
				CLASS(bch2_dev_tryget_noerror, ca)(c, i);
				if (ca)
					prt_printf(&msg.m, " %s", ca->name);
				else
					prt_printf(&msg.m, " dev%u", i);
			}

			guard(mutex_noio)(&c->sb_lock);
			for_each_set_bit(i, devs.d, BCH_SB_MEMBERS_MAX) {
				struct bch_member *m = bch2_members_v2_get_mut(c->disk_sb.sb, i);
				if (m)
					le64_add_cpu(&m->flush_errors, 1);
			}
			bch2_write_super(c);

			if (lost)
				*rewind_seq = range_start;
			/* Not a good range: keep looking further back for lost data */
			nr_good = 0;
		} else if (++nr_good >= 2) {
			/* Two consecutive good ranges — safe to stop */
			break;
		}
	}

	if (!bitmap_empty(lost_nodes.btrees, BTREE_ID_NR)) {
		event_inc_trace(c, journal_scrub_lost_btree_nodes, buf,
				prt_printf(&buf, "%u btree nodes", atomic_read(&lost_nodes.nr)));

		CLASS(bch_log_msg, msg)(c);
		prt_printf(&msg.m, "journal scrub: %u btree nodes with no good replica, "
			   "repairing topology before journal replay\n",
			   atomic_read(&lost_nodes.nr));

		unsigned btree;
		for_each_set_bit(btree, lost_nodes.btrees, BTREE_ID_NR)
			try(bch2_btree_lost_data(c, &msg.m, btree));
	}

	return 0;
}

/*
 * The journal scrub runs during recovery with journal keys frozen - nothing may
 * commit - so what it finds is queued here, for bch2_scrub_journal_do_repairs()
 * once we're rw and any rewind has happened.
 */
void bch2_scrub_journal_queue(struct bch_fs *c, enum btree_id btree, unsigned level,
			      struct bkey_i *k, unsigned bad_devs, int read_err)
{
	static_assert(BKEY_BTREE_PTR_VAL_U64s_MAX <= BKEY_EXTENT_VAL_U64s_MAX);

	scrub_journal_repair r = {
		.btree_id	= btree,
		.level		= level,
		.bad_devs	= bad_devs,
		.read_err	= read_err,
	};
	bkey_copy(&r.k, k);

	guard(mutex)(&c->scrub_journal_repairs_lock);
	darray_push(&c->scrub_journal_repairs, r);
}

enum scrub_journal_outcome {
	SCRUB_JOURNAL_unchanged,
	SCRUB_JOURNAL_repaired,
	SCRUB_JOURNAL_rewound,
	SCRUB_JOURNAL_lost,
	SCRUB_JOURNAL_NR,
};

static int scrub_journal_repair_one(struct moving_context *ctxt,
				    scrub_journal_repair *r,
				    enum scrub_journal_outcome *outcome)
{
	struct btree_trans *trans = ctxt->trans;
	struct bch_fs *c = trans->c;
	struct bkey_s_c stashed = bkey_i_to_s_c(&r->k);

	/*
	 * A btree node with a bad replica: rewrite it from a good one - if the
	 * pointer we found is still the node's. If not, it's been replaced
	 * since, and there's nothing left to repair.
	 */
	if (r->level) {
		*outcome = SCRUB_JOURNAL_repaired;
		int ret = bch2_btree_node_rewrite_key(trans, r->btree_id, r->level - 1, &r->k, 0);
		if (bch2_err_matches(ret, ENOENT)) {
			*outcome = SCRUB_JOURNAL_unchanged;
			ret = 0;
		}
		return ret;
	}

	CLASS(btree_iter, iter)(trans, r->btree_id, bkey_start_pos(stashed.k),
				BTREE_ITER_intent);
	struct bkey_s_c k = bkey_try(bch2_btree_iter_peek_slot(&iter));

	*outcome = SCRUB_JOURNAL_unchanged;

	/*
	 * Gone: the rewind took it, along with the write it came from. If that
	 * write was one we couldn't read back, the file lost it - say so.
	 */
	if (!bch2_extents_match(c, k, stashed)) {
		if (!r->read_err)
			return 0;

		*outcome = SCRUB_JOURNAL_rewound;
		try(bch2_damage_record_key_if_inode(trans, r->btree_id, stashed.k->p,
						    BCH_FSCK_ERR_device_bad_flush_forced_rewind));
		return bch2_trans_commit(trans, NULL, NULL, BCH_TRANS_COMMIT_no_enospc);
	}

	/*
	 * Trim both keys to the overlapping range before remapping,
	 * same as data_update_index_update_key does.
	 */
	struct bkey_i *trimmed = errptr_try(bch2_trans_kmalloc(trans, bkey_bytes(k.k)));
	bkey_reassemble(trimmed, k);
	bch2_cut_front(c, bkey_start_pos(stashed.k), trimmed);
	bch2_cut_back(stashed.k->p, trimmed);

	unsigned bad_devs = ptr_mask_remap(c, stashed, r->bad_devs, bkey_i_to_s_c(trimmed));
	if (!bad_devs)
		return 0;

	/* No good replica, and no rewind took it: that's plain data loss */
	unsigned all_ptrs = 0, ptr_bit = 1;
	bkey_for_each_ptr(bch2_bkey_ptrs(bkey_i_to_s(trimmed)), ptr) {
		all_ptrs |= ptr_bit;
		ptr_bit <<= 1;
	}

	if (r->read_err && bad_devs == all_ptrs) {
		*outcome = SCRUB_JOURNAL_lost;
		try(bch2_damage_record_key(trans, r->btree_id, stashed.k->p,
					   bch2_data_read_sb_err(r->read_err)));
		return bch2_trans_commit(trans, NULL, NULL, BCH_TRANS_COMMIT_no_enospc);
	}

	/* Repair it from whatever replica is good */
	struct bch_inode_opts io_opts;
	try(bch2_bkey_get_io_opts(trans, NULL, k, &io_opts));

	struct data_update_opts data_opts = {
		.type		= BCH_DATA_UPDATE_self_heal,
		.target		= io_opts.background_target,
		.ptrs_io_error	= bad_devs,
		.ptrs_kill	= bad_devs,
	};

	*outcome = SCRUB_JOURNAL_repaired;
	return bch2_move_extent(ctxt, NULL, &io_opts, &data_opts, &iter, 0, k);
}

int bch2_scrub_journal_do_repairs(struct bch_fs *c)
{
	CLASS(darray_scrub_journal_repair, repairs)();

	scoped_guard(mutex, &c->scrub_journal_repairs_lock)
		swap(repairs, c->scrub_journal_repairs);

	if (!repairs.nr)
		return 0;

	struct bch_move_stats stats;
	bch2_move_stats_init(&stats, "journal_scrub_repair");

	struct moving_context ctxt __cleanup(bch2_moving_ctxt_exit);
	bch2_moving_ctxt_init(&ctxt, c, &stats,
			      writepoint_ptr(&c->allocator.reconcile_write_point),
			      true);

	unsigned nr[SCRUB_JOURNAL_NR] = {};

	darray_for_each(repairs, r) {
		enum scrub_journal_outcome outcome = SCRUB_JOURNAL_unchanged;

		/* Ignore errors - don't want them blocking mount */
		lockrestart_do(ctxt.trans, scrub_journal_repair_one(&ctxt, r, &outcome));
		nr[outcome]++;

		if (outcome == SCRUB_JOURNAL_repaired)
			event_inc_trace(c, journal_scrub_repaired, buf,
					bch2_bkey_val_to_text(&buf, c, bkey_i_to_s_c(&r->k)));
	}

	bch_info(c, "journal scrub: repairing %u extents/btree nodes from good replicas; "
		 "%u lost to the rewind, %u with no good replica",
		 nr[SCRUB_JOURNAL_repaired], nr[SCRUB_JOURNAL_rewound], nr[SCRUB_JOURNAL_lost]);
	return 0;
}

void bch2_fs_move_exit(struct bch_fs *c)
{
	darray_exit(&c->scrub_journal_repairs);
}

void bch2_fs_move_init(struct bch_fs *c)
{
	INIT_LIST_HEAD(&c->moving_context_list);
	mutex_init(&c->moving_context_lock);
	darray_init(&c->scrub_journal_repairs);
	mutex_init(&c->scrub_journal_repairs_lock);
}
