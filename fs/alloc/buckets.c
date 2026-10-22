// SPDX-License-Identifier: GPL-2.0
/*
 * Code for manipulating bucket marks for garbage collection.
 *
 * Copyright 2014 Datera, Inc.
 */

#include "bcachefs.h"

#include "alloc/accounting.h"
#include "alloc/background.h"
#include "alloc/backpointers.h"
#include "alloc/buckets.h"
#include "alloc/replicas.h"

#include "btree/bset.h"
#include "btree/check.h"
#include "btree/interior.h"
#include "btree/update.h"

#include "data/copygc.h"
#include "data/ec/trigger.h"
#include "data/extents.h"
#include "data/reconcile/trigger.h"
#include "data/reflink.h"

#include "fs/inode.h"

#include "init/dev.h"
#include "init/error.h"
#include "init/recovery.h"
#include "init/passes.h"

#include "sb/counters.h"

#include <linux/preempt.h>

static u64 reserve_factor(u64 r)
{
	return r + (round_up(r, (1 << RESERVE_FACTOR)) >> RESERVE_FACTOR);
}

void bch2_trans_account_disk_usage_change(struct btree_trans *trans)
{
	struct bch_fs *c = trans->c;

	lockdep_assert_held(&c->capacity.mark_lock.lock);

	u64 disk_res_sectors = trans->disk_res ? trans->disk_res->sectors : 0;
	static int warned_disk_usage = 0;
	bool warn = false;

	struct bch_fs_usage_base *src = &trans->fs_usage_delta;

	s64 added = src->btree + src->data + src->reserved;

	/*
	 * Not allowed to reduce sectors_available except by getting a
	 * reservation:
	 */
	s64 should_not_have_added = added - (s64) disk_res_sectors;
	if (unlikely(should_not_have_added > 0)) {
		/*
		 * We can't tell which slot these came out of, so take them
		 * off all of them: too much self-heals at the next recalc,
		 * too little would overcommit.
		 */
		for (unsigned i = 0; i < BCH_REPLICAS_MAX; i++) {
			u64 old, new;

			old = atomic64_read(&c->capacity.sectors_available[i]);
			do {
				new = max_t(s64, 0, old - should_not_have_added);
			} while (!atomic64_try_cmpxchg(&c->capacity.sectors_available[i],
						       &old, new));
		}

		added -= should_not_have_added;
		warn = true;
	}

	if (added > 0) {
		/* a lost count clamps to slot 0: these were charged elsewhere */
		WARN_ON(!trans->disk_res->nr_replicas);

		unsigned slot = disk_res_slot(trans->disk_res->nr_replicas);

		trans->disk_res->sectors -= added;
		this_cpu_sub(c->capacity.pcpu->online_reserved[slot], added);
	}

	scoped_guard(preempt) {
		struct bch_fs_usage_base *dst = &this_cpu_ptr(c->capacity.pcpu)->usage;
		acc_u64s((u64 *) dst, (u64 *) src, sizeof(*src) / sizeof(u64));
	}

	if (unlikely(warn) && !xchg(&warned_disk_usage, 1))
		bch2_trans_inconsistent(trans,
					"disk usage increased %lli more than %llu sectors reserved)",
					should_not_have_added, disk_res_sectors);
}

/* Mark superblocks: */

static int __bch2_trans_mark_metadata_bucket(struct btree_trans *trans,
				    struct bch_dev *ca, u64 b,
				    enum bch_data_type type,
				    unsigned sectors)
{
	struct bch_fs *c = trans->c;
	CLASS(btree_iter_uninit, iter)(trans);
	int ret = 0;

	struct bkey_i_alloc_v4 *a =
		bch2_trans_start_alloc_update_noupdate(trans, &iter, POS(ca->dev_idx, b));
	if (IS_ERR(a))
		return PTR_ERR(a);

	if (a->v.data_type && type && a->v.data_type != type) {
		CLASS(bch_log_msg, msg)(c);
		prt_printf(&msg.m, "bucket %llu:%llu gen %u different types of data in same bucket: %s, %s\n"
			   "while marking %s\n",
			   iter.pos.inode, iter.pos.offset, a->v.generation,
			   bch2_data_type_str(a->v.data_type),
			   bch2_data_type_str(type),
			   bch2_data_type_str(type));

		bch2_count_fsck_err(c, bucket_metadata_type_mismatch, &msg.m);

		try(bch2_run_explicit_recovery_pass(c, &msg.m,
					BCH_RECOVERY_PASS_check_allocations, 0));

		return bch_err_throw(c, metadata_bucket_inconsistency);
	}

	if (a->v.data_type	!= type ||
	    a->v.dirty_sectors	!= sectors) {
		a->v.data_type		= type;
		a->v.dirty_sectors	= sectors;
		ret = bch2_trans_update(trans, &iter, &a->k_i, 0);
	}

	return ret;
}

static int bch2_mark_metadata_bucket(struct btree_trans *trans, struct bch_dev *ca,
			u64 b, enum bch_data_type data_type, unsigned sectors,
			enum btree_iter_update_trigger_flags flags)
{
	struct bch_fs *c = trans->c;

	struct bucket *g = gc_bucket(ca, b);
	if (bch2_fs_inconsistent_on(!g, c, "reference to invalid bucket on device %u when marking metadata type %s",
				    ca->dev_idx, bch2_data_type_str(data_type)))
		return bch_err_throw(c, metadata_bucket_inconsistency);

	struct bch_alloc_v4 old, new;

	scoped_guard(bucket_lock, g) {
		old = bucket_m_to_alloc(*g);

		if (bch2_fs_inconsistent_on(g->data_type &&
				g->data_type != data_type, c,
				"different types of data in same bucket: %s, %s",
				bch2_data_type_str(g->data_type),
				bch2_data_type_str(data_type)))
			return bch_err_throw(c, metadata_bucket_inconsistency);

		if (bch2_fs_inconsistent_on((u64) g->dirty_sectors + sectors > ca->mi.bucket_size, c,
				"bucket %u:%llu gen %u data type %s sector count overflow: %u + %u > bucket size",
				ca->dev_idx, b, g->generation,
				bch2_data_type_str(g->data_type ?: data_type),
				g->dirty_sectors, sectors))
			return bch_err_throw(c, metadata_bucket_inconsistency);

		g->data_type = data_type;
		g->dirty_sectors += sectors;
		new = bucket_m_to_alloc(*g);
	}

	return bch2_alloc_key_to_dev_counters(trans, ca, &old, &new, flags);
}

int bch2_trans_mark_metadata_bucket(struct btree_trans *trans,
			struct bch_dev *ca, u64 b,
			enum bch_data_type type, unsigned sectors,
			enum btree_iter_update_trigger_flags flags)
{
	BUG_ON(type != BCH_DATA_free &&
	       type != BCH_DATA_sb &&
	       type != BCH_DATA_journal);

	/*
	 * Backup superblock might be past the end of our normal usable space:
	 */
	if (b >= ca->mi.nbuckets)
		return 0;

	if (flags & BTREE_TRIGGER_gc)
		return bch2_mark_metadata_bucket(trans, ca, b, type, sectors, flags);
	else if (flags & BTREE_TRIGGER_transactional)
		return commit_do(trans, NULL, NULL, (unsigned) BCH_WATERMARK_btree,
				 __bch2_trans_mark_metadata_bucket(trans, ca, b, type, sectors));
	else
		BUG();
}

static int bch2_trans_mark_metadata_sectors(struct btree_trans *trans,
			struct bch_dev *ca, u64 start, u64 end,
			enum bch_data_type type, u64 *bucket, unsigned *bucket_sectors,
			enum btree_iter_update_trigger_flags flags)
{
	do {
		u64 b = sector_to_bucket(ca, start);
		unsigned sectors =
			min_t(u64, bucket_to_sector(ca, b + 1), end) - start;

		if (b != *bucket && *bucket_sectors) {
			try(bch2_trans_mark_metadata_bucket(trans, ca, *bucket,
							    type, *bucket_sectors, flags));

			*bucket_sectors = 0;
		}

		*bucket		= b;
		*bucket_sectors	+= sectors;
		start += sectors;
	} while (start < end);

	return 0;
}

static int __bch2_trans_mark_dev_sb(struct btree_trans *trans, struct bch_dev *ca,
			enum btree_iter_update_trigger_flags flags)
{
	struct bch_fs *c = trans->c;
	struct bch_sb_layout layout;

	scoped_guard(mutex_noio, &c->sb_lock)
		layout = ca->disk_sb.sb->layout;

	u64 bucket = 0;
	unsigned i, bucket_sectors = 0;

	for (i = 0; i < layout.nr_superblocks; i++) {
		u64 offset = le64_to_cpu(layout.sb_offset[i]);

		if (offset == BCH_SB_SECTOR)
			try(bch2_trans_mark_metadata_sectors(trans, ca,
						0, BCH_SB_SECTOR,
						BCH_DATA_sb, &bucket, &bucket_sectors, flags));

		try(bch2_trans_mark_metadata_sectors(trans, ca, offset,
				      offset + (1 << layout.sb_max_size_bits),
				      BCH_DATA_sb, &bucket, &bucket_sectors, flags));
	}

	if (bucket_sectors)
		try(bch2_trans_mark_metadata_bucket(trans, ca,
						    bucket, BCH_DATA_sb, bucket_sectors, flags));

	for (i = 0; i < ca->journal.nr; i++)
		try(bch2_trans_mark_metadata_bucket(trans, ca,
				ca->journal.buckets[i],
				BCH_DATA_journal, ca->mi.bucket_size, flags));

	return 0;
}

int bch2_trans_mark_dev_sb(struct bch_fs *c, struct bch_dev *ca,
			enum btree_iter_update_trigger_flags flags)
{
	CLASS(btree_trans, trans)(c);
	int ret = __bch2_trans_mark_dev_sb(trans, ca, flags);
	bch_err_fn(c, ret);
	return ret;
}

int bch2_trans_mark_dev_sbs_flags(struct bch_fs *c,
			enum btree_iter_update_trigger_flags flags)
{
	for_each_online_member(c, ca, BCH_DEV_READ_REF_trans_mark_dev_sbs) {
		/* We unconditionally call bch2_trans_mark_dev_sb() again for an
		 * extra bit of safety; it's harmless if it wasn't needed, and
		 * double allocating a superblock or journal bucket would be
		 * particularly painful
		 */

		int ret = bch2_dev_add_initialize(c, ca) ?:
			  bch2_trans_mark_dev_sb(c, ca, flags);
		if (ret) {
			enumerated_ref_put(&ca->io_ref[READ], BCH_DEV_READ_REF_trans_mark_dev_sbs);
			return ret;
		}
	}

	return 0;
}

int bch2_trans_mark_dev_sbs(struct bch_fs *c)
{
	return bch2_trans_mark_dev_sbs_flags(c, BTREE_TRIGGER_transactional);
}

bool bch2_is_superblock_bucket(struct bch_dev *ca, u64 b)
{
	struct bch_sb_layout *layout = &ca->disk_sb.sb->layout;
	u64 b_offset	= bucket_to_sector(ca, b);
	u64 b_end	= bucket_to_sector(ca, b + 1);
	unsigned i;

	if (!b)
		return true;

	for (i = 0; i < layout->nr_superblocks; i++) {
		u64 offset = le64_to_cpu(layout->sb_offset[i]);
		u64 end = offset + (1 << layout->sb_max_size_bits);

		if (!(offset >= b_end || end <= b_offset))
			return true;
	}

	for (i = 0; i < ca->journal.nr; i++)
		if (b == ca->journal.buckets[i])
			return true;

	return false;
}

/* Disk reservations: */

#define SECTORS_CACHE	1024

/*
 * @eventual includes the slack in partially used buckets of movable types:
 * reserving against it is what makes a reservation mean the write won't run out
 * of space (POSIX, for fallocate), at the price of maybe waiting on copygc.
 * @now is free buckets only - nothing reserves against it, it says whether
 * writes are about to stall.
 *
 * Both are net of ca->reserved_sectors: space the allocator will refuse isn't
 * ours to promise.
 */
struct dev_placeable {
	u64	now;
	u64	eventual;
};

static struct dev_placeable dev_sectors_placeable(struct bch_dev *ca)
{
	struct dev_placeable ret = {};

	if (!dev_has_capacity(ca))
		return ret;

	struct bch_dev_usage_full f = bch2_dev_usage_full_read(ca);
	struct bch_dev_usage u;

	for (unsigned i = 0; i < BCH_DATA_NR; i++)
		u.buckets[i] = f.d[i].buckets;

	u64 reserved = READ_ONCE(ca->reserved_sectors);

	/*
	 * Not __dev_buckets_{free,available}(): those are the allocator's, and
	 * take out open buckets. An open bucket's space is already counted -
	 * as free if nothing's been written to it, as fragmented if something
	 * has - so taking it out again undercounts, until unmount gives it
	 * back. ca->reserved_sectors is our reserve.
	 */
	ret.now		= u.buckets[BCH_DATA_free] * ca->mi.bucket_size;
	ret.eventual	= __dev_buckets_reclaimable(u) * ca->mi.bucket_size;

	for (unsigned i = 0; i < BCH_DATA_NR; i++)
		if (data_type_movable(i))
			ret.eventual += f.d[i].fragmented;

	ret.now		-= min(ret.now, reserved);
	ret.eventual	-= min(ret.eventual, reserved);

	return ret;
}

/*
 * The free space distribution, as much as placeable() needs: the total and the
 * BCH_REPLICAS_MAX - 1 largest devices, and which devices those are.
 *
 * A copy on a device of durability d is min(d, n) of n replicas, so the total
 * is kept at each n, counting each device's free space that many times. A
 * device that can't hold a replica - durability 0, or not rw - isn't in it.
 */
struct dev_free_dist {
	u64		total[BCH_REPLICAS_MAX];
	u64		largest[BCH_REPLICAS_MAX - 1];
	unsigned	largest_dur[BCH_REPLICAS_MAX - 1];
	unsigned	largest_dev[BCH_REPLICAS_MAX - 1];
};

static void dev_free_dist_add(struct dev_free_dist *d, u64 f, unsigned dur, unsigned dev)
{
	if (!dur)
		return;

	for (unsigned n = 1; n <= BCH_REPLICAS_MAX; n++)
		d->total[n - 1] += f * min(dur, n);

	for (unsigned i = 0; i < ARRAY_SIZE(d->largest); i++)
		if (f > d->largest[i]) {
			swap(f, d->largest[i]);
			swap(dur, d->largest_dur[i]);
			swap(dev, d->largest_dev[i]);
		}
}

/*
 * Physical sectors placeable at n replicas. A device holds at most one copy -
 * min(durability, n) of the n - so of P sectors at n replicas the k largest
 * devices, holding h replicas between them, hold at most hP/n, and the rest
 * have to hold what's left:
 *
 *	P(n - h)/n <= what the rest hold		for h < n
 *
 * [2T, 400G, 200G, 200G] at n=3: k=1 leaves 800G for two thirds, k=2 leaves
 * 400G for one third, so 1200G.
 *
 * Reservations are n sectors per sector of data whatever the copies land on -
 * a durability > 1 device doesn't stretch them - so nothing is placeable at n
 * that isn't at fewer: that's what keeps the partition below exclusive.
 */
static u64 dev_free_dist_placeable(const struct dev_free_dist *d, unsigned n)
{
	u64 ret = n > 1 ? dev_free_dist_placeable(d, n - 1) : U64_MAX;
	u64 sum = d->total[n - 1];
	unsigned held = 0;

	BUILD_BUG_ON(ARRAY_SIZE(d->largest) < BCH_REPLICAS_MAX - 1);

	ret = min(ret, sum);

	for (unsigned k = 0; k < ARRAY_SIZE(d->largest); k++) {
		unsigned dur = min(d->largest_dur[k], n);

		held += dur;
		if (held >= n)
			break;

		sum -= min(sum, d->largest[k] * dur);
		ret = min(ret, mul_u64_u64_div_u64(sum, n, n - held));
	}

	return ret;
}

/*
 * At n replicas, a device with more free space than the rest can match - more
 * than the devices smaller than it have between them, over the n - 1 other
 * copies of each write - only fills if every write puts a copy on it: a write
 * that skips it spends the others' space on data it had room for. (For 2x:
 * more free space than all the other devices combined.) The next largest
 * likewise against the smaller ones over n - 2, and so on; it's always a prefix
 * of the n - 1 largest.
 *
 * Detected with hysteresis: we start restricting a margin before we're in
 * trouble, so every device can be filled, and stop only a wider margin after,
 * so we don't flap. @was is the previous result: empty if we weren't.
 *
 * @allowance is how far allocation can move things before the answer could
 * change, in physical sectors: a sector allocated on a smaller device closes
 * the gap by at most one, so allocating less than the gap can't start a
 * restriction. Frees aren't covered - freeing space on the device itself also
 * closes it - but the margin is there for drift.
 */
#define PLACEMENT_ENGAGE_SHIFT		3	/* start 1/8th early */
#define PLACEMENT_RELEASE_SHIFT		2	/* stop 1/4th late */

static bool dev_free_dist_required(const struct dev_free_dist *d, unsigned n,
				   const struct bch_devs_mask *was,
				   struct bch_devs_mask *required,
				   u64 *allowance)
{
	unsigned held = 0;
	bool ret = false;

	memset(required, 0, sizeof(*required));
	*allowance = U64_MAX;

	for (unsigned i = 0; i < ARRAY_SIZE(d->largest); i++) {
		u64 f = d->largest[i];
		unsigned dev = d->largest_dev[i];
		unsigned dur = min(d->largest_dur[i], n);

		/* replicas each write still needs from smaller devices */
		if (held + dur >= n)
			break;
		unsigned need = n - held - dur;

		u64 smaller = d->total[need - 1];
		for (unsigned j = 0; j <= i; j++)
			smaller -= min(smaller, d->largest[j] * min(d->largest_dur[j], need));

		u64 have	= f * need;
		u64 engage	= smaller - (smaller >> PLACEMENT_ENGAGE_SHIFT);
		u64 release	= smaller - (smaller >> PLACEMENT_RELEASE_SHIFT);

		if (have > (test_bit(dev, was->d) ? release : engage)) {
			__set_bit(dev, required->d);
			held += dur;
			ret = true;
			continue;
		}

		/* nothing smaller to spend: allocation can't close the gap */
		if (smaller)
			*allowance = max(engage - have,
					 smaller >> (PLACEMENT_ENGAGE_SHIFT + 1));
		break;
	}

	return ret;
}

/*
 * Free space partitioned by the highest replica count it supports: out[n-1] is
 * placeable at n and not n+1, so no two slots describe the same sectors.
 * Outstanding reservations come off the way the slowpath drew them, from their
 * slot upwards; @unattributed has no count, so it comes off the top.
 */
static void dev_free_dist_partition(const struct dev_free_dist *d,
				    const u64 *reserved, u64 unattributed,
				    u64 *out)
{
	u64 above = 0;

	for (unsigned n = BCH_REPLICAS_MAX; n >= 1; --n) {
		u64 placeable = dev_free_dist_placeable(d, n);

		/* non-increasing in n, or this underflows */
		EBUG_ON(placeable < above);

		out[n - 1]	= placeable - above;
		above		= placeable;
	}

	/*
	 * Most constrained first: the top slot's reservations can only
	 * have come from the top slot. What doesn't fit was
	 * overcommitted, and is dropped.
	 */
	for (int k = BCH_REPLICAS_MAX - 1; k >= 0; --k) {
		u64 todo = reserve_factor(reserved[k]);

		for (unsigned i = k; i < BCH_REPLICAS_MAX && todo; i++) {
			u64 take = min(out[i], todo);

			out[i]	-= take;
			todo	-= take;
		}
	}

	unattributed = reserve_factor(unattributed);

	for (unsigned n = BCH_REPLICAS_MAX; n >= 1 && unattributed; --n) {
		u64 take = min(out[n - 1], unattributed);

		out[n - 1]	-= take;
		unattributed	-= take;
	}

	for (unsigned n = 0; n < BCH_REPLICAS_MAX; n++)
		out[n] = avail_factor(out[n]);
}

static void __bch2_fs_sectors_placeable(struct bch_fs *c, u64 *out, u64 *out_now,
					bool update_placement)
{
	struct dev_free_dist eventual = {}, now = {};

	scoped_guard(rcu)
		for_each_member_device_rcu(c, ca, NULL) {
			struct dev_placeable f = dev_sectors_placeable(ca);

			dev_free_dist_add(&eventual, f.eventual, bch2_dev_rw_durability(ca), ca->dev_idx);
			dev_free_dist_add(&now, f.now, bch2_dev_rw_durability(ca), ca->dev_idx);
		}

	if (update_placement) {
		u64 allowance = U64_MAX;

		for (unsigned n = 2; n <= BCH_REPLICAS_MAX; n++) {
			struct bch_devs_mask *cur = &c->capacity.placement_required[n - 1];
			struct bch_devs_mask required;
			u64 a;

			bool constrained = dev_free_dist_required(&eventual, n, cur, &required, &a);

			*cur = required;
			mod_bit(n - 1, &c->capacity.placement_constrained, constrained);
			allowance = min(allowance, a);
		}

		c->capacity.placement_allowance = allowance;
	}

	/* reserved but not yet written, so not in the bucket counts */
	struct bch_fs_capacity_pcpu b;
	percpu_u64s_get_nonnegative((u64 *) &b, (u64 __percpu *) c->capacity.pcpu,
				    sizeof(b) / sizeof(u64));

	dev_free_dist_partition(&eventual, b.online_reserved, b.usage.reserved, out);
	if (out_now)
		dev_free_dist_partition(&now, b.online_reserved, b.usage.reserved, out_now);
}

void bch2_fs_sectors_placeable(struct bch_fs *c, u64 *out, u64 *out_now)
{
	guard(percpu_read_noio)(&c->capacity.mark_lock);
	__bch2_fs_sectors_placeable(c, out, out_now, false);

	for (int i = BCH_REPLICAS_MAX - 2; i >= 0; --i) {
		out[i] += out[i + 1];
		if (out_now)
			out_now[i] += out_now[i + 1];
	}
}

/*
 * What the fastpath may hand out before the next recompute: never more than
 * placement_allowance, so the placement restriction is re-evaluated before
 * allocation could have changed it. Capped here and not in the recalc's
 * decision - running out forces a recompute, never an -ENOSPC. Most capable
 * slot first: a reservation at any count can draw from above its own.
 */
static void disk_reservation_caches_store(struct bch_fs *c, const u64 *avail)
{
	u64 budget = c->capacity.placement_allowance;

	for (int i = BCH_REPLICAS_MAX - 1; i >= 0; --i) {
		u64 v = min(avail[i], budget);

		budget -= v;
		atomic64_set(&c->capacity.sectors_available[i], v);
	}
}

/*
 * Every slot, not just ours: consuming at one count shrinks the others, and not
 * linearly, so they only stay honest recomputed together.
 */
static void disk_reservation_caches_reset(struct bch_fs *c, u64 *avail)
{
	lockdep_assert_held(&c->capacity.sectors_available_lock);

	int cpu;
	for_each_possible_cpu(cpu)
		for (unsigned i = 0; i < BCH_REPLICAS_MAX; i++)
			WRITE_ONCE(per_cpu_ptr(c->capacity.pcpu, cpu)->sectors_available[i], 0);

	__bch2_fs_sectors_placeable(c, avail, NULL, true);
}

void bch2_disk_reservation_degraded(struct bch_fs *c, unsigned wanted,
				    unsigned got, u64 sectors, unsigned long ip)
{
	event_add_trace(c, disk_reservation_degraded, sectors, buf,
		prt_printf(&buf, "wanted %u replicas, got %u: %llu sectors from %pS",
			   wanted, got, sectors, (void *) ip));
}

/* Device state changed: the caches were computed against the old devices */
void bch2_disk_reservation_caches_invalidate(struct bch_fs *c)
{
	u64 avail[BCH_REPLICAS_MAX];

	guard(spinlock)(&c->capacity.sectors_available_lock);
	disk_reservation_caches_reset(c, avail);

	disk_reservation_caches_store(c, avail);
}

static int disk_reservation_recalc_sectors_available(struct bch_fs *c,
			struct disk_reservation *res,
			u64 sectors, enum bch_reservation_flags flags)
{
	guard(spinlock)(&c->capacity.sectors_available_lock);

	unsigned slot = disk_res_slot(res->nr_replicas);
	u64 avail[BCH_REPLICAS_MAX], have = 0;

	disk_reservation_caches_reset(c, avail);

	/* anything placeable at more replicas than we need will do */
	for (unsigned i = slot; i < BCH_REPLICAS_MAX; i++)
		have += avail[i];

	if (have && (flags & BCH_DISK_RESERVATION_PARTIAL))
		sectors = min(sectors, have);

	bool ok = sectors <= have || (flags & BCH_DISK_RESERVATION_NOFAIL);

	if (ok) {
		u64 todo = sectors;

		/* least capable space first - see dev_free_dist_partition() */
		for (unsigned i = slot; i < BCH_REPLICAS_MAX && todo; i++) {
			u64 take = min(todo, avail[i]);

			avail[i]	-= take;
			todo		-= take;
		}
	}

	disk_reservation_caches_store(c, avail);

	/* after a shutdown nothing is writable: that's not out of space */
	if (!ok)
		return test_bit(BCH_FS_emergency_ro, &c->flags)
			? bch_err_throw(c, emergency_ro)
			: bch_err_throw(c, ENOSPC_disk_reservation);

	this_cpu_add(c->capacity.pcpu->online_reserved[slot], sectors);
	res->sectors			+= sectors;
	return 0;
}

int bch2_disk_reservation_add_slowpath(struct bch_fs *c, struct disk_reservation *res,
				u64 sectors, enum bch_reservation_flags flags)
{
	guard(preempt)();
	struct bch_fs_capacity_pcpu *pcpu = this_cpu_ptr(c->capacity.pcpu);
	unsigned slot = disk_res_slot(res->nr_replicas);
	u64 want = sectors + SECTORS_CACHE;

	/*
	 * Refill from our slot upwards, least capable first, leaving
	 * scarce high-replica space to writes with no alternative. It
	 * all lands in our slot's cache: a per-pool cache would never hit
	 * for 1x writes, since slot 1 is empty wherever every device can
	 * hold a copy.
	 */
	for (unsigned i = slot;
	     i < BCH_REPLICAS_MAX && pcpu->sectors_available[slot] < want;
	     i++) {
		u64 need = want - pcpu->sectors_available[slot], get;
		u64 old = atomic64_read(&c->capacity.sectors_available[i]);

		do {
			get = min(need, old);
		} while (get &&
			 !atomic64_try_cmpxchg(&c->capacity.sectors_available[i],
					       &old, old - get));

		pcpu->sectors_available[slot] += get;
	}

	/*
	 * A recalc on another cpu may zero our cache: read it once, so the
	 * subtract is from what we checked.
	 */
	u64 avail = READ_ONCE(pcpu->sectors_available[slot]);

	/* the recalc zeroes the caches, so what we drew above isn't lost */
	if (unlikely(avail < sectors))
		return disk_reservation_recalc_sectors_available(c, res, sectors, flags);

	WRITE_ONCE(pcpu->sectors_available[slot], avail - sectors);
	pcpu->online_reserved[slot]	+= sectors;
	res->sectors			+= sectors;
	return 0;
}

/* Startup/shutdown: */

void bch2_buckets_nouse_free(struct bch_fs *c)
{
	for_each_member_device(c, ca) {
		kvfree_rcu_mightsleep(ca->buckets_nouse);
		ca->buckets_nouse = NULL;
	}
}

int bch2_buckets_nouse_alloc(struct bch_fs *c)
{
	for_each_member_device(c, ca) {
		BUG_ON(ca->buckets_nouse);

		ca->buckets_nouse = bch2_kvmalloc(BITS_TO_LONGS(ca->mi.nbuckets) *
					    sizeof(unsigned long),
					    GFP_KERNEL|__GFP_ZERO);
		if (!ca->buckets_nouse)
			return bch_err_throw(c, ENOMEM_buckets_nouse);
	}

	return 0;
}

static void bucket_gens_free_rcu(struct rcu_head *rcu)
{
	struct bucket_gens *buckets =
		container_of(rcu, struct bucket_gens, rcu);

	kvfree(buckets);
}
DEFINE_FREE(bucket_gens_free, struct bucket_gens *, if (_T) call_rcu(&_T->rcu, bucket_gens_free_rcu));

int bch2_dev_buckets_resize(struct bch_fs *c, struct bch_dev *ca, u64 nbuckets)
{
	struct bucket_gens *bucket_gens __free(bucket_gens_free) = NULL, *old_bucket_gens = NULL;
	bool resize = ca->bucket_gens != NULL;

	if (resize)
		lockdep_assert_held(&c->state_lock);

	if (resize && ca->buckets_nouse)
		return bch_err_throw(c, no_resize_with_buckets_nouse);

	bucket_gens = bch2_kvmalloc(struct_size(bucket_gens, b, nbuckets),
				    GFP_KERNEL|__GFP_ZERO);
	if (!bucket_gens)
		return bch_err_throw(c, ENOMEM_bucket_gens);

	bucket_gens->first_bucket = ca->mi.first_bucket;
	bucket_gens->nbuckets	= nbuckets;
	bucket_gens->nbuckets_minus_first =
		bucket_gens->nbuckets - bucket_gens->first_bucket;

	old_bucket_gens = rcu_dereference_protected(ca->bucket_gens, 1);

	if (resize) {
		u64 copy = min(bucket_gens->nbuckets,
			       old_bucket_gens->nbuckets);
		memcpy(bucket_gens->b,
		       old_bucket_gens->b,
		       sizeof(bucket_gens->b[0]) * copy);
	}

	try(bch2_bucket_bitmap_resize(ca, &ca->bucket_backpointer_mismatch, nbuckets));
	try(bch2_bucket_bitmap_resize(ca, &ca->bucket_backpointer_empty, nbuckets));

	rcu_assign_pointer(ca->bucket_gens, bucket_gens);
	bucket_gens	= old_bucket_gens;
	nbuckets	= ca->mi.nbuckets;

	return 0;
}

void bch2_dev_buckets_free(struct bch_dev *ca)
{
	kvfree(ca->buckets_nouse);
	kvfree(rcu_dereference_protected(ca->bucket_gens, 1));
	free_percpu(ca->usage);
}

int bch2_dev_buckets_alloc(struct bch_fs *c, struct bch_dev *ca)
{
	ca->usage = alloc_percpu(struct bch_dev_usage_full);
	if (!ca->usage)
		return bch_err_throw(c, ENOMEM_usage_init);

	return bch2_dev_buckets_resize(c, ca, ca->mi.nbuckets);
}

/* For Rust: static inlines over percpu counters */
int rust_bch2_disk_reservation_add(struct bch_fs *c, struct disk_reservation *res,
				   u64 sectors, unsigned nr_replicas, int flags)
{
	return bch2_disk_reservation_add(c, res, sectors, nr_replicas, flags);
}

void rust_bch2_disk_reservation_put(struct bch_fs *c, struct disk_reservation *res)
{
	bch2_disk_reservation_put(c, res);
}
