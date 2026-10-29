/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_ALLOC_FOREGROUND_H
#define _BCACHEFS_ALLOC_FOREGROUND_H

#include "bcachefs.h"
#include "alloc/buckets.h"
#include "alloc/types_gen.h"
#include "btree/iter.h"
#include "data/extents.h"
#include "data/write_gen.h"
#include "sb/members.h"

#include <linux/hash.h>

#include "alloc/foreground_gen.h"
#include "alloc/dev_select_gen.h"
#include "alloc/bucket_alloc_gen.h"
#include "alloc/sector_alloc_gen.h"
#include "alloc/write_point_gen.h"

extern const char * const bch2_watermarks[];

static inline struct bch_dev *ob_dev(struct bch_fs *c, struct open_bucket *ob)
{
	return bch2_dev_have_ref(c, ob->dev);
}

static inline unsigned bch2_open_buckets_reserved(enum bch_watermark watermark)
{
	switch (watermark) {
	case BCH_WATERMARK_interior_updates:
		return 0;
	case BCH_WATERMARK_reclaim:
		return OPEN_BUCKETS_COUNT / 6;
	case BCH_WATERMARK_btree:
	case BCH_WATERMARK_btree_copygc:
		return OPEN_BUCKETS_COUNT / 4;
	case BCH_WATERMARK_copygc:
		return OPEN_BUCKETS_COUNT / 3;
	default:
		return OPEN_BUCKETS_COUNT / 2;
	}
}

/*
 * Lowest watermark the open bucket pool can still serve; 0 = not starved.
 *
 * Linear walk: the reserve only shrinks as priority rises, so there is one
 * crossover. Stops one short of BCH_WATERMARK_NR to leave interior updates a
 * way through. The reserves tie at some rungs (stripe/normal,
 * btree/btree_copygc), so those are never returned.
 */
static inline unsigned bch2_open_buckets_starved_watermark(struct bch_fs *c)
{
	unsigned nr_free = READ_ONCE(c->allocator.open_buckets_nr_free), w = 0;

	while (w < BCH_WATERMARK_NR - 1 &&
	       nr_free < bch2_open_buckets_reserved(w))
		w++;

	return w;
}

/*
 * freelist_wait wake helpers. Every wake_up site on
 * c->allocator.freelist_wait should go through these so the wake counters
 * are maintained in lockstep with waitlist wakeups — they're the signal
 * waiters use to filter spurious wakes they don't care about.
 *
 * _dev: bump one device's alloc_wake_counter and wake (use when the caller
 *       knows which device changed state in a way that might unblock
 *       allocs). Waiters retry only if a device in their failed-alloc trace
 *       advanced.
 * _all: bump the fs-wide wake_all_counter and wake (use for events that
 *       might unblock allocs on any device — capacity/device changes,
 *       journal state changes, fsck progress, debug knobs). This forces a
 *       full allocator retry, since eligibility may have changed for a
 *       device outside any waiter's trace (e.g. a newly added device).
 * _waiters_unpark: wake without bumping any counter; used to drop our own
 *       closure off the waitlist when we can't continue waiting. Real
 *       waiters see no counter advance and re-park silently.
 */

static inline void ob_push(struct bch_fs *c, struct open_buckets *obs,
			   struct open_bucket *ob)
{
	BUG_ON(obs->nr >= ARRAY_SIZE(obs->v));

	obs->v[obs->nr++] = ob - c->allocator.open_buckets;
}

#define open_bucket_for_each(_c, _obs, _ob, _i)					\
	for ((_i) = 0;								\
	     (_i) < (_obs)->nr &&						\
	     ((_ob) = (_c)->allocator.open_buckets + (_obs)->v[_i], true);	\
	     (_i)++)

static inline struct open_bucket *ec_open_bucket(struct bch_fs *c,
						 struct open_buckets *obs)
{
	struct open_bucket *ob;
	unsigned i;

	open_bucket_for_each(c, obs, ob, i)
		if (ob->ec)
			return ob;

	return NULL;
}

static inline void bch2_open_bucket_put(struct bch_fs *c, struct open_bucket *ob)
{
	if (atomic_dec_and_test(&ob->pin))
		__bch2_open_bucket_put(c, ob);
}

static inline void bch2_open_buckets_put(struct bch_fs *c,
					 struct open_buckets *ptrs)
{
	struct open_bucket *ob;
	unsigned i;

	open_bucket_for_each(c, ptrs, ob, i)
		bch2_open_bucket_put(c, ob);
	ptrs->nr = 0;
}

static inline void bch2_alloc_sectors_done_inlined(struct bch_fs *c, struct write_point *wp)
{
	struct open_buckets ptrs = { .nr = 0 }, keep = { .nr = 0 };
	struct open_bucket *ob;
	unsigned i;

	open_bucket_for_each(c, &wp->ptrs, ob, i)
		ob_push(c, ob->sectors_free < block_sectors(c)
			? &ptrs
			: &keep, ob);
	wp->ptrs = keep;

	unsigned sectors = wp->prev_sectors_free - wp->sectors_free;
	event_add_trace(c, sectors_alloc, sectors, buf, ({
		prt_str(&buf, bch2_data_type_str(wp->data_type));
	}));

	mutex_unlock(&wp->lock);

	bch2_open_buckets_put(c, &ptrs);
}

/*
 * Give up on the open buckets in @wp that have less than @sectors free, so the
 * next allocation gets fresh ones instead of handing the same too-small write
 * point straight back.
 *
 * For writes that can't be split and so need that much contiguous room: a btree
 * node, or a compressed extent we can't decompress. The tail of each retired
 * bucket is fragmentation, which copygc reclaims.
 *
 * Only ask for something a fresh bucket can hold, or the caller loops.
 */
static inline void bch2_alloc_sectors_retire_short(struct bch_fs *c,
						   struct write_point *wp,
						   unsigned sectors)
{
	struct open_bucket *ob;
	unsigned i;

	open_bucket_for_each(c, &wp->ptrs, ob, i)
		if (ob->sectors_free < sectors)
			ob->sectors_free = 0;

	bch2_alloc_sectors_done_inlined(c, wp);
}

static inline void bch2_open_bucket_get(struct bch_fs *c,
					struct write_point *wp,
					struct open_buckets *ptrs)
{
	struct open_bucket *ob;
	unsigned i;

	open_bucket_for_each(c, &wp->ptrs, ob, i) {
		ob->data_type = wp->data_type;
		atomic_inc(&ob->pin);
		ob_push(c, ptrs, ob);
	}
}

static inline open_bucket_idx_t *open_bucket_hashslot(struct bch_fs *c,
						  unsigned dev, u64 bucket)
{
	return c->allocator.open_buckets_hash + bch2_open_bucket_hash(dev, bucket);
}

static inline struct open_bucket *bch2_bucket_is_open(struct bch_fs *c, unsigned dev, u64 bucket)
{
	open_bucket_idx_t slot = *open_bucket_hashslot(c, dev, bucket);

	while (slot) {
		struct open_bucket *ob = &c->allocator.open_buckets[slot];

		if (ob->dev == dev && ob->bucket == bucket)
			return ob;

		slot = ob->hash;
	}

	return NULL;
}

static inline bool bch2_bucket_is_open_safe(struct bch_fs *c, unsigned dev, u64 bucket)
{
	if (bch2_bucket_is_open(c, dev, bucket))
		return true;

	guard(spinlock)(&c->allocator.freelist_lock);
	return bch2_bucket_is_open(c, dev, bucket);
}



DEFINE_FREE(alloc_request_put, struct alloc_request *,
	    if (!IS_ERR_OR_NULL(_T)) alloc_request_put(_T))

static inline int bch2_alloc_sectors_start_trans(struct btree_trans *trans,
			     unsigned target,
			     unsigned erasure_code,
			     struct write_point_specifier write_point,
			     struct bch_devs_list *devs_have,
			     unsigned nr_replicas,
			     unsigned ec_replicas,
			     enum bch_watermark watermark,
			     enum bch_write_flags flags,
			     struct closure *cl,
			     struct write_point **wp_ret)
{
	struct alloc_request *req = errptr_try(alloc_request_get(trans, target, erasure_code,
								 devs_have,
								 nr_replicas,
								 ec_replicas,
								 watermark, flags, cl));
	int ret = bch2_alloc_sectors_req(trans, req, write_point, wp_ret);
	alloc_request_put(req);
	return ret;
}

static inline struct bch_extent_ptr bch2_ob_ptr(struct bch_fs *c, struct open_bucket *ob)
{
	struct bch_dev *ca = ob_dev(c, ob);

	return (struct bch_extent_ptr) {
		.type	= 1 << BCH_EXTENT_ENTRY_ptr,
		.generation	= ob->generation,
		.dev	= ob->dev,
		.offset	= bucket_to_sector(ca, ob->bucket) +
			ca->mi.bucket_size -
			ob->sectors_free,
	};
}

/*
 * Append pointers to the space we just allocated to @k, and mark @sectors space
 * as allocated out of @ob
 */
static inline void
bch2_alloc_sectors_append_ptrs_inlined(struct bch_fs *c, struct write_point *wp,
				       struct bkey_i *k, unsigned sectors,
				       bool cached)
{
	struct open_bucket *ob;
	unsigned i;

	BUG_ON(sectors > wp->sectors_free);
	wp->sectors_free	-= sectors;
	wp->sectors_allocated	+= sectors;

	open_bucket_for_each(c, &wp->ptrs, ob, i) {
		struct bch_dev *ca = ob_dev(c, ob);
		struct bch_extent_ptr ptr = bch2_ob_ptr(c, ob);

		ptr.cached = cached ||
			(!ca->mi.durability &&
			 wp->data_type == BCH_DATA_user);

		bch2_bkey_append_ptr(c, k, ptr);

		BUG_ON(sectors > ob->sectors_free);
		ob->sectors_free -= sectors;
	}
}

void bch2_alloc_sectors_append_ptrs(struct bch_fs *, struct write_point *,
				    struct bkey_i *, unsigned, bool);
void bch2_alloc_sectors_done(struct bch_fs *, struct write_point *);

static inline struct write_point_specifier writepoint_hashed(unsigned long v)
{
	return (struct write_point_specifier) { .v = v | 1 };
}

static inline struct write_point_specifier writepoint_ptr(struct write_point *wp)
{
	return (struct write_point_specifier) { .v = (unsigned long) wp };
}


/*
 * Returns an error if the filesystem has gone emergency read-only: the
 * allocator won't hand out buckets again, so callers must not retry. A normal
 * read-only transition still allocates (for the final flush), and keeps waiting.
 */
static inline int bch2_wait_on_allocator(struct btree_trans *trans,
					 struct alloc_request *req,
					 int err,
					 struct closure *cl)
{
	if (closure_nr_remaining(cl) > 1)
		__bch2_wait_on_allocator(trans, req, err, cl);

	return test_bit(BCH_FS_emergency_ro, &trans->c->flags)
		? bch_err_throw(trans->c, emergency_ro)
		: 0;
}

#endif /* _BCACHEFS_ALLOC_FOREGROUND_H */
