/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_SNAPSHOT_TYPES_H
#define _BCACHEFS_SNAPSHOT_TYPES_H

#include <linux/percpu-rwsem.h>
#include <linux/rwsem.h>

#include "btree/bbpos_types.h"
#include "init/progress.h"
#include "util/darray.h"

DEFINE_DARRAY_NAMED(snapshot_id_list, u32);

#define IS_ANCESTOR_BITMAP	128

/*
 * In-memory snapshot table entry, indexed by snapshot ID.
 *
 * Snapshots form a binary tree where IDs decrease going deeper: a parent's ID
 * is always greater than its children's.
 *
 * Ancestor lookups use a three-tier strategy:
 *  1. Skiplist (skip[]): jump up the tree in O(log n) steps
 *  2. Bitmap (is_ancestor[]): O(1) lookup for ancestors within 128 IDs
 *  3. Parent walk: fallback linear traversal
 *
 * Read under RCU; partial is_ancestor[] updates are tolerable since readers
 * fall back to the skiplist.
 */
struct snapshot_t {
	enum snapshot_id_state {
		SNAPSHOT_ID_empty,
		SNAPSHOT_ID_live,
		SNAPSHOT_ID_deleted,
	}			state;
	u32			parent;
	/* skiplist: random ancestors, sorted ascending; try [2] first */
	u32			skip[3];
	u32			depth;
	u32			children[2];	/* normalized: [0] >= [1] */
	u32			subvol; /* Nonzero only if a subvolume points to this node: */
	u32			tree;
	/* bit (ancestor - id - 1) set for ancestors within 128 IDs */
	unsigned long		is_ancestor[BITS_TO_LONGS(IS_ANCESTOR_BITMAP)];
};

struct snapshot_table {
	struct rcu_head		rcu;
	size_t			nr;
#ifndef RUST_BINDGEN
	DECLARE_FLEX_ARRAY(struct snapshot_t, s);
#else
	struct snapshot_t	s[0];
#endif
};

struct snapshot_interior_delete {
	u32	id;
	u32	live_child;
};
DEFINE_DARRAY_NAMED(interior_delete_list, struct snapshot_interior_delete);

struct snapshot_delete {
	struct mutex			progress_lock;
	snapshot_id_list		deleting_from_trees;
	snapshot_id_list		delete_leaves;
	interior_delete_list		delete_interior;
	interior_delete_list		no_keys;
	interior_delete_list		eytzinger_delete_list;

	bool				running;
	unsigned			version;
	struct progress_indicator	progress;
};

/*
 * Snapshot creation must prevent userspace from dirtying the page cache while
 * the snapshot is being taken: sync_inodes_sb flushes existing dirty pages
 * before the snapshot transaction, but if new pages get dirtied in the window
 * between sync_inodes_sb returning and the snapshot transaction running, those
 * dirty pages can be partially flushed (e.g. data page flushed but redo log
 * page not yet) such that the snapshot captures an inconsistent state — the
 * shape that bit MySQL/InnoDB.
 *
 * Page-cache dirtying paths take these locks as readers; snapshot creation
 * takes them as writers. O_DIRECT doesn't need them — direct writes commit as
 * atomic btree transactions, no page cache staleness window. Buffered
 * writeback is fine too — each writeback insert is atomic w.r.t. the
 * snapshot transaction.
 *
 * There are two locks because the two dirtying paths sit on opposite sides
 * of mmap_lock, mirroring the sb_writers freeze levels (SB_FREEZE_WRITE vs
 * SB_FREEZE_PAGEFAULT):
 *
 *   write_iter:   create_lock -> mmap_lock (faulting in user pages)
 *   page_mkwrite: mmap_lock (held by caller) -> pagefault_lock
 *
 * One lock can't sit both above and below mmap_lock — that's an ABBA
 * inversion. Snapshot creation write-locks create_lock first (draining
 * syscall-level dirtiers, whose user-page faults may still take
 * pagefault_lock as readers), then pagefault_lock (draining mmap dirtiers).
 * Lock order: create_lock -> mmap_lock -> pagefault_lock.
 */
struct bch_fs_snapshots {
	struct snapshot_table __rcu		*table;
	struct mutex				table_lock;
	/* a topology repair invalidated descendants' is_ancestor bitmaps: */
	bool					need_table_rebuild;
	struct percpu_rw_semaphore		create_lock;
	struct percpu_rw_semaphore		pagefault_lock;
	struct snapshot_delete			delete;
	struct delayed_work			wait_for_pagecache_and_delete_work;
	snapshot_id_list			unlinked;
	struct mutex				unlinked_lock;
};

typedef struct {
	/* we can't have padding in this struct: */
	u64		subvol;
	u64		inum;
} subvol_inum;

#endif /* _BCACHEFS_SNAPSHOT_TYPES_H */
