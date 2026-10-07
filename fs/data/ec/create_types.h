/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_DATA_EC_CREATE_TYPES_H
#define _BCACHEFS_DATA_EC_CREATE_TYPES_H

#include "enum_kind.h"

struct ec_dev_stripe_state {
	struct list_head	list;
	struct mutex		lock;

	unsigned		disk_label;

	struct dev_stripe_state	block_stripe;
	struct dev_stripe_state	parity_stripe;
};

enum __enum_closed ec_stripe_ref {
	STRIPE_REF_io,
	STRIPE_REF_stripe,
	STRIPE_REF_NR
};

/*
 * open:	the sector allocator can still hand blocks out of this stripe;
 *		it's the stripe head's h->s.
 * filling:	every block has been handed to a writer, but the writers haven't
 *		finished - buckets are still checked out, and the stripe can
 *		only be completed by the writers that already hold them.
 * in_flight:	every bucket has come back, the data is all in, and creation is
 *		queued on ec.stripe_create_wq. This is the only state guaranteed
 *		to make progress on its own.
 *
 * The filling/in_flight split is what makes it possible to wait on stripe
 * buffer memory safely: a stripe that is filling may never complete (its
 * writers can go away), so blocking on one can deadlock, while an in_flight
 * stripe always drains.
 */
#define EC_STRIPE_NEW_STATES()			\
	x(open)					\
	x(filling)				\
	x(in_flight)

enum __enum_closed ec_stripe_new_state {
#define x(n)	EC_STRIPE_NEW_##n,
	EC_STRIPE_NEW_STATES()
#undef x
	EC_STRIPE_NEW_STATE_NR
};

struct ec_stripe_new_bucket {
	struct hlist_node	hash;
	u64			dev_bucket;
};

struct ec_stripe_new {
	struct bch_fs		*c;
	struct moving_context	*ctxt;
	struct mutex		lock;
	struct list_head	list;
	struct work_struct	work;
	struct closure		cl;

	atomic_t		ref[STRIPE_REF_NR];

	/* seq is only assigned once the refs are gone, so it can't give an age */
	u64			start_time;
	u64			seq;

	int			err;

	/*
	 * Set by ec_old_stripe_fold() from the read's completion, read by
	 * create: its own field so neither side needs a lock to say what
	 * happened. @old_stripe_lost_blocks is the blocks that were carried forward
	 * and are unreadable, i.e. the data the failure actually cost us.
	 */
	int			old_stripe_err;
	u32			old_stripe_lost_blocks;

	struct bch_devs_mask	devs;
	enum bch_watermark	watermark;
	enum ec_stripe_new_state state;

	bool			have_old_stripe:1;

	bool			allocated:1;
	bool			mem_allocated:1;
	bool			old_stripe_read:1;
	bool			old_stripe_read_all:1;

	unsigned long		blocks_gotten[BITS_TO_LONGS(BCH_BKEY_PTRS_MAX)];
	unsigned long		blocks_allocated[BITS_TO_LONGS(BCH_BKEY_PTRS_MAX)];
	unsigned long		blocks_moving[BITS_TO_LONGS(BCH_BKEY_PTRS_MAX)];
	open_bucket_idx_t	blocks[BCH_BKEY_PTRS_MAX];
	struct disk_reservation	res;

	struct ec_stripe_new_bucket buckets[BCH_BKEY_PTRS_MAX];

	struct ec_stripe_buf	new_stripe;
	struct ec_stripe_buf	old_stripe;

	struct ec_stripe_handle	new_stripe_handle;
	struct ec_stripe_handle	old_stripe_handle;

	u8			old_block_map[BCH_BKEY_PTRS_MAX];
	u8			old_blocks_nr;
};

struct ec_stripe_head {
	struct list_head	list;
	struct mutex		lock;

	unsigned		disk_label;
	unsigned		algo;
	unsigned		redundancy;
	enum bch_watermark	watermark;
	bool			insufficient_devs;

	unsigned long		rw_devs_change_count;

	u64			nr_created;

	struct bch_devs_mask	devs;
	unsigned		nr_active_devs;

	unsigned		blocksize;

	struct ec_dev_stripe_state *dev_stripe;

	struct ec_stripe_new	*s;
};

/*
 * Lazy per-(disk_label, sectors) cache of RW member counts (the can_widen
 * target); shared by callers that walk stripes and need the widening target
 * many times (scan, check). Kept eytzinger-sorted between inserts.
 */
struct widen_cache_entry {
	u8	disk_label;
	u16	sectors;
	u16	nr_devs;
};

DEFINE_DARRAY_NAMED(widen_cache, struct widen_cache_entry);

struct alloc_request;

struct moving_context;

struct ec_stripe_buf;

#endif /* _BCACHEFS_DATA_EC_CREATE_TYPES_H */
