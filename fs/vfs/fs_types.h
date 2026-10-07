/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_VFS_FS_TYPES_H
#define _BCACHEFS_VFS_FS_TYPES_H

#include "enum_kind.h"

struct bch_inode_info {
	struct inode		v;
	struct rhash_head	hash;
	/*
	 * Read it with inode_inum(); the layout is private so that
	 * bch2_inode_or_descendents_is_open() can walk entries without knowing
	 * about bch_inode_info at all.
	 */
	struct bch_inum_hash_entry ei_inum_hash;

	/*
	 * Cached extent allocation state for [start, end), for skipping btree
	 * lookups when initializing bch_folio state in sequential buffered
	 * writes.
	 *
	 * Staleness contract: extents may change toward allocated without
	 * notice (worst case we over-reserve); anything that deallocates must
	 * clear the range, under pagecache_block so no fill (under
	 * pagecache_add) can straddle the deallocation. Fillers build their
	 * result in locals and publish it in one go: fillers aren't
	 * serialized against each other (page_mkwrite fills without i_rwsem),
	 * and incremental publishing would let two fills interleave into a
	 * range neither of them scanned.
	 */
	u64			ei_reserved_start;
	u64			ei_reserved_end;
	u8			ei_reserved_replicas;
	u8			ei_reserved_state;
	spinlock_t		ei_reserved_lock;

	unsigned		ei_inodes_idx;
	unsigned long		ei_flags;

	struct mutex		ei_update_lock;
	u64			ei_quota_reserved;
	unsigned long		ei_last_dirtied;
	two_state_lock_t	ei_pagecache_lock;

	struct mutex		ei_quota_lock;
	struct bch_qid		ei_qid;

	/*
	 * When we've been doing nocow writes we'll need to issue flushes to the
	 * underlying block devices
	 *
	 * XXX: a device may have had a flush issued by some other codepath. It
	 * would be better to keep for each device a sequence number that's
	 * incremented when we isusue a cache flush, and track here the sequence
	 * number that needs flushing.
	 */
	struct bch_devs_mask	ei_devs_need_flush;

	/* copy of inode in btree: */
	struct bch_inode_unpacked ei_inode;

	struct delayed_work	ei_writeback_timer;
};

enum __enum_flags bch_inode_lock_op {
	INODE_PAGECACHE_BLOCK	= (1U << 0),
	INODE_UPDATE_LOCK	= (1U << 1),
};

struct bch_inode_unpacked;

#ifndef NO_BCACHEFS_FS

/* returns 0 if we want to do the update, or error is passed up */
typedef int (*inode_set_fn)(struct btree_trans *,
			    struct bch_inode_info *,
			    struct bch_inode_unpacked *, void *);

#endif

#endif /* _BCACHEFS_VFS_FS_TYPES_H */
