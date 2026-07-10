// SPDX-License-Identifier: GPL-2.0
/*
 * Copyright 2012 Google, Inc.
 */

/* DOC(copygc)
 *
 * As a copy-on-write filesystem, bcachefs never overwrites data in place.
 * Random overwrites leave buckets partially empty — the old data is obsolete
 * but still occupies disk space until the bucket is reclaimed. The copying
 * garbage collector (copygc) handles this: it finds the most fragmented
 * buckets, relocates their remaining live data, and frees the buckets for
 * reuse. This is automatic and continuous.
 *
 * Performance near full: copygc needs free space to relocate data into, so a
 * portion of free space is reserved exclusively for it (`copygc_reserve`, 8%
 * by default, configurable 5-21%). Normal writes cannot dip into this reserve.
 * As the filesystem fills beyond the reserve threshold, write latency
 * increases because new writes must wait for copygc to free space first. This
 * is the primary reason to avoid running a bcachefs filesystem above ~90%
 * capacity under write-heavy workloads.
 *
 * A fragmentation LRU btree tracks bucket fill levels so copygc can
 * efficiently find the worst buckets without scanning the entire allocation
 * state. Copygc reads live data from selected buckets (located via
 * backpointers), writes it to new buckets, and updates extent pointers
 * atomically.
 */

#include "bcachefs.h"

#include "alloc/accounting.h"
#include "alloc/background.h"
#include "alloc/backpointers.h"
#include "alloc/buckets.h"
#include "alloc/foreground.h"
#include "alloc/lru.h"

#include "btree/iter.h"
#include "btree/update.h"
#include "btree/write_buffer.h"

#include "data/ec/trigger.h"
#include "data/move.h"
#include "data/copygc.h"

#include "init/error.h"

#include "sb/counters.h"

#include "util/clock.h"

#include <linux/freezer.h>
#include <linux/kthread.h>
#include <linux/math64.h>
#include <linux/sched/task.h>
#include <linux/sort.h>
#include <linux/wait.h>

struct buckets_in_flight {
	struct rhashtable	*table;
	struct move_bucket	*first;
	struct move_bucket	*last;
	size_t			nr;
	size_t			sectors;

	DARRAY(struct move_bucket *) to_evacuate;
};

static const struct rhashtable_params bch_move_bucket_params = {
	.head_offset		= offsetof(struct move_bucket, hash),
	.key_offset		= offsetof(struct move_bucket, k),
	.key_len		= sizeof(struct move_bucket_key),
	.automatic_shrinking	= true,
};

static void move_bucket_in_flight_add(struct buckets_in_flight *list, struct move_bucket *b)
{
	if (!list->first)
		list->first = b;
	else
		list->last->next = b;

	list->last = b;
	list->nr++;
	list->sectors += b->sectors;
}

static int bch2_bucket_is_movable(struct btree_trans *trans,
				  struct move_bucket *b, u64 time)
{
	struct bch_fs *c = trans->c;

	/*
	 * Valid bucket?
	 *
	 * XXX: we should kill the LRU entry here if it's not
	 */
	CLASS(bch2_dev_bucket_tryget, ca)(c, b->k.bucket);
	if (!ca)
		return 0;

	if (ca->mi.state != BCH_MEMBER_STATE_rw ||
	    !bch2_dev_is_online(ca)) {
		bch_err_throw(c, bucket_not_moveable_dev_not_rw);
		return 0;
	}

	/* Bucket still being written? */
	if (bch2_bucket_is_open(c, b->k.bucket.inode, b->k.bucket.offset)) {
		bch_err_throw(c, bucket_not_moveable_bucket_open);
		return 0;
	}

	/* We won't be able to evacuate it if there's missing backpointers */
	if (bch2_bucket_bitmap_test(&ca->bucket_backpointer_mismatch, b->k.bucket.offset)) {
		bch_err_throw(c, bucket_not_moveable_bp_mismatch);
		return 0;
	}

	CLASS(btree_iter, iter)(trans, BTREE_ID_alloc, b->k.bucket, BTREE_ITER_cached);
	struct bkey_s_c k = bkey_try(bch2_btree_iter_peek_slot(&iter));

	struct bch_alloc_v4 _a;
	const struct bch_alloc_v4 *a = bch2_alloc_to_v4(k, &_a);
	b->k.generation	= a->generation;
	b->sectors	= bch2_bucket_sectors_dirty(*a);
	u64 lru_idx	= alloc_lru_idx_fragmentation(*a, ca);

	if (!lru_idx || lru_idx > time) {
		bch_err_throw(c, bucket_not_moveable_lru_race);
		return 0;
	}

	return true;
}

static void move_bucket_free(struct buckets_in_flight *list,
			     struct move_bucket *b)
{
	int ret = rhashtable_remove_fast(list->table, &b->hash,
					 bch_move_bucket_params);
	BUG_ON(ret);
	kfree(b);
}

static void move_buckets_wait(struct moving_context *ctxt,
			      struct buckets_in_flight *list,
			      bool flush)
{
	struct move_bucket *i;

	while ((i = list->first)) {
		if (flush)
			move_ctxt_wait_event(ctxt, !atomic_read(&i->count));

		if (atomic_read(&i->count))
			break;

		list->first = i->next;
		if (!list->first)
			list->last = NULL;

		list->nr--;
		list->sectors -= i->sectors;

		move_bucket_free(list, i);
	}

	bch2_trans_unlock_long(ctxt->trans);
}

static bool bucket_in_flight(struct buckets_in_flight *list,
			     struct move_bucket_key k)
{
	return rhashtable_lookup_fast(list->table, &k, bch_move_bucket_params);
}

static bool copygc_batch_full(struct buckets_in_flight *buckets_in_flight)
{
	size_t nr_to_get = max_t(size_t, 16U, buckets_in_flight->nr / 4);

	return buckets_in_flight->to_evacuate.nr >= nr_to_get;
}

/* Returns 1 if the bucket was added to the batch, 0 if skipped: */
static int try_add_copygc_bucket(struct btree_trans *trans,
				 struct buckets_in_flight *buckets_in_flight,
				 struct bpos bucket, u64 lru_time)
{
	struct move_bucket b = { .k.bucket = bucket };

	int ret = bch2_bucket_is_movable(trans, &b, lru_time);
	if (ret <= 0)
		return ret;

	if (bucket_in_flight(buckets_in_flight, b.k))
		return 0;

	struct move_bucket *b_i = kmalloc(sizeof(*b_i), GFP_KERNEL);
	if (!b_i)
		return -ENOMEM;

	*b_i = b;

	ret = darray_push(&buckets_in_flight->to_evacuate, b_i);
	if (ret) {
		kfree(b_i);
		return ret;
	}

	ret = rhashtable_lookup_insert_fast(buckets_in_flight->table, &b_i->hash,
					    bch_move_bucket_params);
	BUG_ON(ret);

	return 1;
}

struct copygc_dev {
	unsigned	dev;
	s64		wait;
	struct bpos	pos;
	/* lru exhausted, or the device no longer needs copygc: */
	bool		done;
};

DEFINE_DARRAY_NAMED(darray_copygc_dev, struct copygc_dev)

static int copygc_dev_cmp(const void *_l, const void *_r)
{
	const struct copygc_dev *l = _l, *r = _r;

	return cmp_int(l->wait, r->wait);
}

/*
 * The devices that currently need copygc - fragmented space over their
 * allowance, i.e. wait amount exhausted - sorted neediest first. When the list
 * is empty, *wait is the amount of io (in sectors, by the write io clock)
 * until the closest device will need it.
 */
static int copygc_dev_list(struct bch_fs *c, darray_copygc_dev *devs, u64 *wait)
{
	devs->nr = 0;
	*wait = U64_MAX;

	try(darray_make_room(devs, c->sb.nr_devices));

	scoped_guard(percpu_read, &c->capacity.mark_lock)
		scoped_guard(rcu)
			for_each_rw_member_rcu(c, ca) {
				s64 v = bch2_copygc_dev_wait_amount(ca);

				/* No allocating under rcu - skip if a device raced in: */
				if (v <= 0 && devs->nr < devs->size)
					darray_push(devs, ((struct copygc_dev) {
						.dev	= ca->dev_idx,
						.wait	= v,
					}));
				else if (v > 0)
					*wait = min(*wait, (u64) v);
			}

	sort(devs->data, devs->nr, sizeof(devs->data[0]), copygc_dev_cmp, NULL);

	if (devs->nr)
		*wait = 0;
	return 0;
}

/*
 * Get one bucket from this device's fragmentation lru, resuming from where we
 * left off. Returns 1 if a bucket was added to the batch, 0 if the lru is
 * exhausted:
 */
static int copygc_dev_get_bucket(struct moving_context *ctxt,
			struct buckets_in_flight *buckets_in_flight,
			struct copygc_dev *d)
{
	struct btree_trans *trans = ctxt->trans;

	int ret = for_each_btree_key_max(trans, iter, BTREE_ID_lru,
				  d->pos,
				  lru_end(bucket_fragmentation_lru(d->dev)),
				  0, k, ({
		int ret2 = try_add_copygc_bucket(trans, buckets_in_flight,
					      u64_to_bucket(k.k->p.offset),
					      lru_pos_time(k.k->p));
		d->pos = bpos_successor(k.k->p);
		ret2;
	}));

	if (!ret)
		d->done = true;
	return ret;
}

/*
 * In-flight evacuations completing and reconcile moving data off can bring a
 * device back under its allowance mid batch - pop it off the list when it no
 * longer needs copygc:
 */
static bool copygc_dev_still_needed(struct bch_fs *c, struct copygc_dev *d)
{
	guard(percpu_read)(&c->capacity.mark_lock);
	guard(rcu)();
	struct bch_dev *ca = bch2_dev_rcu_noerror(c, d->dev);

	return ca && bch2_copygc_can_make_progress(ca);
}

static int bch2_copygc_get_buckets(struct moving_context *ctxt,
			struct buckets_in_flight *buckets_in_flight,
			darray_copygc_dev *devs)
{
	darray_for_each(*devs, i) {
		i->pos	= lru_start(bucket_fragmentation_lru(i->dev));
		i->done	= false;
	}

	/*
	 * Round robin among the devices that need evacuating, one bucket per
	 * device per pass, so that no single device monopolizes the batch and
	 * every pressured device makes progress:
	 */
	unsigned done;
	do {
		done = 0;

		darray_for_each(*devs, i) {
			if (!i->done &&
			    !copygc_dev_still_needed(ctxt->trans->c, i))
				i->done = true;

			if (i->done) {
				done++;
				continue;
			}

			int ret = copygc_dev_get_bucket(ctxt, buckets_in_flight, i);
			if (ret < 0)
				return ret;

			if (copygc_batch_full(buckets_in_flight))
				return 0;
		}
	} while (done < devs->nr);

	return 0;
}

static int bch2_copygc_get_stripe_buckets(struct moving_context *ctxt,
			struct buckets_in_flight *buckets_in_flight)
{
	struct btree_trans *trans = ctxt->trans;

	int ret = for_each_btree_key_max(trans, iter, BTREE_ID_lru,
				  lru_start(BCH_LRU_STRIPE_FRAGMENTATION),
				  lru_end(BCH_LRU_STRIPE_FRAGMENTATION),
				  0, lru_k, ({
		CLASS(btree_iter, s_iter)(trans, BTREE_ID_stripes, POS(0, lru_k.k->p.offset), 0);
		struct bkey_s_c s_k = bch2_btree_iter_peek_slot(&s_iter);
		int ret2 = bkey_err(s_k);
		if (ret2)
			goto err;

		if (s_k.k->type != KEY_TYPE_stripe)
			continue;

		const struct bch_stripe *s = bkey_s_c_to_stripe(s_k).v;

		/* write buffer race? */
		if (stripe_lru_pos(s) != lru_pos_time(lru_k.k->p))
			continue;

		unsigned nr_data = s->nr_blocks - s->nr_redundant;
		for (unsigned i = 0; i < nr_data; i++) {
			if (!stripe_blockcount_get(s, i))
				continue;

			const struct bch_extent_ptr *ptr = s->ptrs + i;
			CLASS(bch2_dev_bkey_tryget, ca)(trans->c, s_k, ptr->dev);
			if (unlikely(!ca))
				continue;

			ret2 = try_add_copygc_bucket(trans, buckets_in_flight,
						     PTR_BUCKET_POS(ca, ptr), U64_MAX);
			if (ret2 < 0)
				break;

			ret2 = copygc_batch_full(buckets_in_flight);
			if (ret2)
				break;
		}
err:
		ret2;
	}));

	return ret < 0 ? ret : 0;
}

static bool should_do_ec_copygc(struct btree_trans *trans, darray_copygc_dev *devs)
{
	u64 stripe_frag_ratio = 0;

	for_each_btree_key_max(trans, iter, BTREE_ID_lru,
			       lru_start(BCH_LRU_STRIPE_FRAGMENTATION),
			       lru_end(BCH_LRU_STRIPE_FRAGMENTATION),
			       0, lru_k, ({
		CLASS(btree_iter, s_iter)(trans, BTREE_ID_stripes, POS(0, lru_k.k->p.offset), 0);
		struct bkey_s_c s_k = bch2_btree_iter_peek_slot(&s_iter);
		int ret = bkey_err(s_k);
		if (ret)
			goto err;

		if (s_k.k->type != KEY_TYPE_stripe)
			continue;

		const struct bch_stripe *s = bkey_s_c_to_stripe(s_k).v;

		/* write buffer race? */
		if (stripe_lru_pos(s) != lru_pos_time(lru_k.k->p))
			continue;

		unsigned nr_data = s->nr_blocks - s->nr_redundant, blocks_nonempty = 0;
		for (unsigned i = 0; i < nr_data; i++)
			blocks_nonempty += !!stripe_blockcount_get(s, i);

		/* stripe is pending delete */
		if (!blocks_nonempty)
			continue;

		/* This matches the calculation in alloc_lru_idx_fragmentation, so we can
		 * directly compare without actually looking up the bucket pointed to by the
		 * bucket fragmentation lru:
		 */
		stripe_frag_ratio = div_u64(blocks_nonempty * (1ULL << 31), nr_data);
		break;
err:
		ret;
	}));

	/*
	 * Compare against the best bucket candidate this round can actually
	 * evacuate - the emptiest lru head across the devices that need
	 * copygc:
	 */
	u64 bucket_frag_ratio = 0;
	darray_for_each(*devs, i) {
		u16 lru_id = bucket_fragmentation_lru(i->dev);

		CLASS(btree_iter, iter)(trans, BTREE_ID_lru, lru_start(lru_id), 0);
		struct bkey_s_c lru_k;
		struct bpos lru_end_pos = lru_end(lru_id);

		lockrestart_do(trans, bkey_err(lru_k = bch2_btree_iter_peek_max(&iter, &lru_end_pos)));

		if (lru_k.k && !bkey_err(lru_k)) {
			u64 t = lru_pos_time(lru_k.k->p);

			if (!bucket_frag_ratio || t < bucket_frag_ratio)
				bucket_frag_ratio = t;
		}
	}

	/* Prefer normal bucket copygc */
	return stripe_frag_ratio && stripe_frag_ratio * 2 < bucket_frag_ratio;
}

noinline
static int bch2_copygc(struct moving_context *ctxt,
		       struct buckets_in_flight *buckets_in_flight,
		       darray_copygc_dev *devs,
		       bool *did_work)
{
	struct btree_trans *trans = ctxt->trans;
	struct bch_fs *c = trans->c;
	struct data_update_opts data_opts = {
		.type		= BCH_DATA_UPDATE_copygc,
		.commit_flags	= (unsigned) BCH_WATERMARK_copygc,
	};
	u64 sectors_seen	= atomic64_read(&ctxt->stats->sectors_seen);
	u64 sectors_moved	= atomic64_read(&ctxt->stats->sectors_moved);
	int ret = 0;

	move_buckets_wait(ctxt, buckets_in_flight, false);

	ret = bch2_btree_write_buffer_tryflush(trans);
	if (bch2_err_matches(ret, EROFS))
		goto err;

	if (bch2_fs_fatal_err_on(ret, c, "%s: from bch2_btree_write_buffer_tryflush()", bch2_err_str(ret)))
		goto err;

	ret = should_do_ec_copygc(trans, devs)
		? bch2_copygc_get_stripe_buckets(ctxt, buckets_in_flight)
		: bch2_copygc_get_buckets(ctxt, buckets_in_flight, devs);
	if (ret)
		goto err;

	darray_for_each(buckets_in_flight->to_evacuate, i) {
		if (kthread_should_stop() || freezing(current))
			break;

		struct move_bucket *b = *i;
		*i = NULL;

		move_bucket_in_flight_add(buckets_in_flight, b);
		bch2_moving_ctxt_reset_limits(ctxt);
		/* Copygc walks physical source buckets, unlike target-directed reconcile. */
		if (bch2_dev_rotational(c, b->k.bucket.inode))
			bch2_moving_ctxt_set_rotational_limits(ctxt,
					MOVE_ROTATIONAL_LIMIT_background,
					MOVE_LIMITS_COPYGC_BUCKET,
					b->k.bucket.inode);

		ret = bch2_evacuate_bucket(ctxt, b, b->k.bucket, b->k.generation, data_opts);
		if (ret)
			goto err;

		*did_work = true;
	}
err:
	/* no entries in LRU btree found, or got to end: */
	if (bch2_err_matches(ret, ENOENT))
		ret = 0;

	if (ret < 0 && !bch2_err_matches(ret, EROFS))
		bch_err_msg(c, ret, "from bch2_move_data()");

	sectors_seen	= atomic64_read(&ctxt->stats->sectors_seen) - sectors_seen;
	sectors_moved	= atomic64_read(&ctxt->stats->sectors_moved) - sectors_moved;
	event_inc_trace(c, copygc, buf, ({
		prt_printf(&buf, "buckets %zu sectors seen %llu moved %llu",
			   buckets_in_flight->to_evacuate.nr, sectors_seen, sectors_moved);
	}));

	darray_for_each(buckets_in_flight->to_evacuate, i)
		if (*i)
			move_bucket_free(buckets_in_flight, *i);
	darray_exit(&buckets_in_flight->to_evacuate);
	return ret;
}

/*
 * Will copygc run on this device? The allocator uses this on the blocked path
 * to decide whether to kick copygc and wait for it, or bail: it must be the
 * same criterion copygc uses to build its device list, so the allocator never
 * waits on a copygc run that isn't coming - and never bails when one is.
 */
bool bch2_copygc_can_make_progress(struct bch_dev *ca)
{
	return bch2_copygc_dev_wait_amount(ca) <= 0;
}

/*
 * Returns how much io (in sectors, by the write io clock) until this device
 * will need copygc: <= 0 means it needs it now, and the magnitude is how far
 * past its fragmented-space allowance it is - the sort key for picking which
 * device needs copygc the most.
 *
 * Caller must hold mark_lock (read), for the dev_leaving accounting read -
 * and must take it outside any rcu read section, mark_lock can block.
 *
 * The allowance at the limit - when the device is full - is the space we
 * reserved in bch2_recalc_capacity; we can't have more than that amount of
 * disk space stranded due to fragmentation and store everything we have
 * promised to store. But we don't want to be running copygc unnecessarily
 * when the device still has plenty of free space - rather, we want copygc to
 * smoothly run every so often and continually reduce the amount of fragmented
 * space as the device fills up - so we increase the allowance by half the
 * current free space.
 */
s64 bch2_copygc_dev_wait_amount(struct bch_dev *ca)
{
	struct bch_fs *c = ca->fs;
	struct bch_dev_usage_full usage_full = bch2_dev_usage_full_read(ca);
	struct bch_dev_usage usage;

	for (unsigned i = 0; i < BCH_DATA_NR; i++)
		usage.buckets[i] = usage_full.d[i].buckets;

	/*
	 * Sectors that reconcile is scheduled to move off this device count as
	 * free-to-be: a full device whose data is mostly leaving doesn't need
	 * copygc, it needs reconcile to run.
	 */
	struct disk_accounting_pos pos;
	disk_accounting_key_init(pos, dev_leaving, .dev = ca->dev_idx);
	s64 leaving;
	bch2_accounting_mem_read_locked(c, disk_accounting_pos_to_bpos(&pos), &leaving, 1);
	leaving = max(0LL, leaving);

	/* Don't start until less than 20% of the device is free: */
	s64 free = usage.buckets[BCH_DATA_free] * ca->mi.bucket_size + leaving;
	s64 wait = free * 5 - ca->mi.nbuckets * ca->mi.bucket_size;
	if (wait > 0)
		return wait;

	s64 fragmented_allowed = ((__dev_buckets_free(ca, usage, BCH_WATERMARK_stripe) +
				   bch2_dev_buckets_reserved(ca, BCH_WATERMARK_stripe)) *
				  ca->mi.bucket_size + leaving) >> 1;
	s64 fragmented = 0;

	for (unsigned i = 0; i < BCH_DATA_NR; i++)
		if (data_type_movable(i))
			fragmented += usage_full.d[i].fragmented;

	return fragmented_allowed - fragmented;
}

__cold void bch2_copygc_wait_to_text(struct printbuf *out, struct bch_fs *c)
{
	printbuf_tabstop_push(out, 32);
	prt_printf(out, "running:\t%u\n",		c->copygc.running);
	prt_printf(out, "run count:\t%u\n",		c->copygc.run_count);
	prt_printf(out, "copygc_wait:\t%llu\n",		c->copygc.wait);
	prt_printf(out, "copygc_wait_at:\t%llu\n",	c->copygc.wait_at);

	prt_printf(out, "Currently waiting for:\t");
	prt_human_readable_u64(out, max(0LL, c->copygc.wait -
					atomic64_read(&c->io_clock[WRITE].now)) << 9);
	prt_newline(out);

	prt_printf(out, "Currently waiting since:\t");
	prt_human_readable_u64(out, max(0LL,
					atomic64_read(&c->io_clock[WRITE].now) -
					c->copygc.wait_at) << 9);
	prt_newline(out);

	bch2_printbuf_make_room(out, 4096);

	struct task_struct *t;
	scoped_guard(percpu_read, &c->capacity.mark_lock)
	scoped_guard(rcu) {
		guard(printbuf_atomic)(out);
		prt_printf(out, "Currently calculated wait:\n");
		for_each_rw_member_rcu(c, ca) {
			prt_printf(out, "  %s:\t", ca->name);
			prt_human_readable_s64(out, bch2_copygc_dev_wait_amount(ca));
			prt_newline(out);
		}

		t = rcu_dereference(c->copygc.thread);
		if (t)
			get_task_struct(t);
	}

	if (t) {
		bch2_prt_task_backtrace(out, t, 0, GFP_KERNEL);
		put_task_struct(t);
	}
}

static int bch2_copygc_thread(void *arg)
{
	struct bch_fs *c = arg;
	struct moving_context ctxt;
	struct bch_move_stats move_stats;
	struct io_clock *clock = &c->io_clock[WRITE];
	struct buckets_in_flight buckets = {};
	CLASS(darray_copygc_dev, devs)();
	u64 last, wait;
	u32 kick = c->copygc.kick_count;

	buckets.table = kzalloc(sizeof(*buckets.table), GFP_KERNEL);
	int ret = !buckets.table
		? -ENOMEM
		: rhashtable_init(buckets.table, &bch_move_bucket_params);
	bch_err_msg(c, ret, "allocating copygc buckets in flight");
	if (ret)
		goto err;

	set_freezable();

	/*
	 * Data move operations can't run until after check_snapshots has
	 * completed, and bch2_snapshot_is_ancestor() is available.
	 */
	kthread_wait_freezable(c->recovery.pass_done > BCH_RECOVERY_PASS_check_snapshots ||
			       kthread_should_stop());
	if (kthread_should_stop())
		goto out;

	bch2_move_stats_init(&move_stats, "copygc");
	bch2_moving_ctxt_init(&ctxt, c, NULL, &move_stats,
			      writepoint_ptr(&c->copygc.write_point),
			      false);

	while (!ret && !kthread_should_stop()) {
		bool did_work = false;

		bch2_trans_unlock_long(ctxt.trans);
		cond_resched();

		if (!c->opts.copygc_enabled) {
			move_buckets_wait(&ctxt, &buckets, true);
			kthread_wait_freezable(c->opts.copygc_enabled ||
					       kthread_should_stop());
		}

		if (unlikely(freezing(current))) {
			move_buckets_wait(&ctxt, &buckets, true);
			__refrigerator(false);
			continue;
		}

		last = atomic64_read(&clock->now);
		ret = copygc_dev_list(c, &devs, &wait);
		if (ret)
			break;

		if (!devs.nr &&
		    kick == READ_ONCE(c->copygc.kick_count)) {
			c->copygc.wait_at = last;
			c->copygc.wait = last + wait;
			move_buckets_wait(&ctxt, &buckets, true);

			/*
			 * Recheck the kick after setting TASK_INTERRUPTIBLE:
			 * the allocator's kick + wake_up_process() is either
			 * seen here or wakes the sleep - no lost wakeups (the
			 * io clock only advances with write throughput, which
			 * may be stalled on the kicker):
			 */
			set_current_state(TASK_INTERRUPTIBLE);
			if (kick == READ_ONCE(c->copygc.kick_count))
				bch2_kthread_io_clock_wait_once(clock, last + wait,
						MAX_SCHEDULE_TIMEOUT);
			__set_current_state(TASK_RUNNING);
			continue;
		}

		kick = READ_ONCE(c->copygc.kick_count);
		c->copygc.wait = 0;

		c->copygc.running = true;
		ret = bch2_copygc(&ctxt, &buckets, &devs, &did_work);
		c->copygc.running = false;
		c->copygc.run_count++;

		wake_up(&c->copygc.running_wq);

		if (!wait && !did_work) {
			u64 min_member_capacity = bch2_min_rw_member_capacity(c);

			if (min_member_capacity == U64_MAX)
				min_member_capacity = 128 * 2048;

			move_buckets_wait(&ctxt, &buckets, true);

			set_current_state(TASK_INTERRUPTIBLE);
			if (kick == READ_ONCE(c->copygc.kick_count))
				bch2_kthread_io_clock_wait_once(clock, last + (min_member_capacity >> 6),
						MAX_SCHEDULE_TIMEOUT);
			__set_current_state(TASK_RUNNING);
		}
	}

	move_buckets_wait(&ctxt, &buckets, true);
	bch2_moving_ctxt_exit(&ctxt);
	bch2_move_stats_exit(&move_stats, c);
out:
	rhashtable_destroy(buckets.table);
err:
	kfree(buckets.table);
	return ret;
}

void bch2_copygc_stop(struct bch_fs *c)
{
	struct task_struct *t = rcu_dereference_protected(c->copygc.thread, true);
	if (t) {
		kthread_stop(t);
		put_task_struct(t);
	}
	c->copygc.thread = NULL;
}

int bch2_copygc_start(struct bch_fs *c)
{
	if (c->opts.nochanges)
		return 0;

	if (bch2_fs_init_fault("copygc_start"))
		return -ENOMEM;

	if (!c->copygc.wq &&
	    !(c->copygc.wq = alloc_workqueue("bcachefs_copygc",
				WQ_HIGHPRI|WQ_FREEZABLE|WQ_MEM_RECLAIM|WQ_UNBOUND, 1)))
		return bch_err_throw(c, ENOMEM_fs_other_alloc);

	if (!c->copygc.thread) {
		struct task_struct *t =
			kthread_create(bch2_copygc_thread, c, "bch-copygc/%s", c->name);
		int ret = PTR_ERR_OR_ZERO(t);
		bch_err_msg(c, ret, "creating copygc thread");
		if (ret)
			return ret;

		get_task_struct(t);

		c->copygc.thread = t;
		rcu_assign_pointer(c->copygc.thread, t);
		wake_up_process(c->copygc.thread);
	}

	return 0;
}

void bch2_fs_copygc_exit(struct bch_fs *c)
{
	if (c->copygc.wq)
		destroy_workqueue(c->copygc.wq);
}

void bch2_fs_copygc_init(struct bch_fs *c)
{
	init_waitqueue_head(&c->copygc.running_wq);
	c->copygc.running = false;
}
