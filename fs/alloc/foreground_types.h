/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_ALLOC_FOREGROUND_TYPES_H
#define _BCACHEFS_ALLOC_FOREGROUND_TYPES_H

#include "enum_kind.h"

struct bkey;

struct bch_dev;

struct bch_fs;

struct bch_devs_List;

struct dev_alloc_list {
	unsigned	nr;
	u8		data[BCH_SB_MEMBERS_MAX];
};

typedef struct {
	u8	dev;
	bool	new_stripe_alloc:1;
	bool	will_retry_all_devices:1;
	bool	will_retry_target_devices:1;
	bool	will_retry_set_devices:1;
	bool	copygc_can_make_progress:1;
	bool	have_cl:1;
	s16	err;
	u32	wake_counter_snapshot;
	u64	free_buckets;
} alloc_trace_entry;

struct alloc_request {
	struct closure		*cl;
	u32			wake_all_counter_snapshot;
	u8			nr_replicas;
	u8			ec_replicas;
	u8			ec_max_data_blocks;	/* 0 = no cap */
	unsigned		target;
	bool			ec:1;
	bool			new_stripe_alloc:1;
	bool			will_retry_all_devices:1;
	bool			will_retry_target_devices:1;
	bool			will_retry_set_devices:1;
	bool			copygc_can_make_progress:1;
	bool			trace_alloc_failed:1;
	/* failure domains are a hard requirement, not a preference (erasure coding): */
	bool			failure_domains_required:1;
	enum bch_watermark	watermark;
	enum bch_write_flags	flags;
	enum bch_data_type	data_type;
	struct bch_devs_list	*devs_have;
	struct write_point	*wp;

	/* These fields are used primarily by open_bucket_add_buckets */
	struct open_buckets	ptrs;
	unsigned		nr_effective;	/* sum of @ptrs durability */
	struct bch_devs_mask	devs_may_alloc;

	/*
	 * Devices already holding a replica of what we're allocating for -
	 * devs_have plus buckets allocated so far - for spreading replicas
	 * across failure domains, see bch2_dev_domain_key():
	 */
	struct bch_devs_mask	devs_chosen;

	/* devices this allocation should use first, see bch2_dev_alloc_required() */
	const struct bch_devs_mask *devs_required;

	/* bch2_bucket_alloc_set_trans(): */
	struct dev_alloc_list	devs_sorted;
	u64			domain_keys[BCH_SB_MEMBERS_MAX];
	struct bch_dev_usage	usage;

	/* bch2_bucket_alloc_trans(): */
	struct bch_dev		*ca;

	/*
	 * Allocate the free bucket nearest this device position (a 32.32
	 * fixed point fraction of the device, see dev_frac_to_offset()),
	 * instead of allocating from the device cursor; 0 = no target.
	 *
	 * A fraction rather than a sector offset so it means the same thing
	 * on devices of different sizes: erasure coding uses it to allocate
	 * a stripe's blocks at equivalent positions on each device, and the
	 * device is chosen after the target is set:
	 */
	u64			target_frac;

	enum __enum_closed {
				BTREE_BITMAP_NO,
				BTREE_BITMAP_YES,
				BTREE_BITMAP_ANY,
	}			btree_bitmap;

	struct {
		u64		buckets_seen;
		u64		skipped_open;
		u64		skipped_need_journal_commit;
		u64		need_journal_commit;
		u64		skipped_nocow;
		u64		skipped_nouse;
		u64		skipped_mi_btree_bitmap;
	} counters;

	unsigned		scratch_nr_replicas;
	unsigned		scratch_nr_effective;
	enum bch_write_flags	scratch_flags;
	bool			scratch_have_cache;
	enum bch_data_type	scratch_data_type;
	struct open_buckets	scratch_ptrs;
	struct bch_devs_mask	scratch_devs_may_alloc;

	/* Allocation attempt trace — dumped on allocator stuck */
	DARRAY_PREALLOCATED(alloc_trace_entry, 16) trace;
};

enum bch_write_flags;

#endif /* _BCACHEFS_ALLOC_FOREGROUND_TYPES_H */
