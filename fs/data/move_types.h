/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_MOVE_TYPES_H
#define _BCACHEFS_MOVE_TYPES_H

#include "btree/bbpos_types.h"
#include "bcachefs_ioctl.h"
#include "init/dev_types.h"
#include "util/darray.h"

struct bch_move_stats {
	char			name[32];
	bool			phys;
	enum bch_ioctl_data_event_ret	ret;

	union {
	struct {
		enum bch_data_type	data_type;
		struct bbpos		pos;
	};
	struct {
		unsigned		dev;
		u64			offset;
	};
	};

	atomic64_t		keys_moved;
	atomic64_t		keys_raced;
	atomic64_t		sectors_seen;
	atomic64_t		sectors_moved;
	atomic64_t		sectors_raced;
	atomic64_t		sectors_error_corrected;
	atomic64_t		sectors_error_uncorrected;
	struct bch_devs_mask	devs_error_uncorrected;
};

struct move_bucket_key {
	struct bpos		bucket;
	unsigned		generation;
};

struct move_bucket {
	struct move_bucket	*next;
	struct rhash_head	hash;
	struct move_bucket_key	k;
	unsigned		sectors;
	atomic_t		count;
};

/*
 * What the journal scrub found wrong with an extent, for
 * bch2_scrub_journal_do_repairs() once we're rw: the scrub runs with journal
 * keys frozen, so it can neither repair nor record damage itself.
 *
 * @bad_devs is a mask of pointers by their position in @k, like ptrs_io_error,
 * not of device indices. @read_err is set when reading them failed outright,
 * rather than a replica being found bad on a read that succeeded.
 */
typedef struct {
	enum btree_id		btree_id;
	unsigned		bad_devs;
	int			read_err;
	__BKEY_PADDED(k, BKEY_EXTENT_VAL_U64s_MAX);
} scrub_journal_repair;

DEFINE_DARRAY(scrub_journal_repair);

#endif /* _BCACHEFS_MOVE_TYPES_H */
