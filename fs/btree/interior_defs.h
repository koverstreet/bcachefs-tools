/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_BTREE_INTERIOR_DEFS_H
#define _BCACHEFS_BTREE_INTERIOR_DEFS_H

#include "enum_kind.h"

#define BTREE_UPDATE_NODES_MAX		((BTREE_MAX_DEPTH - 2) * 2 + GC_MERGE_NODES)

#define BTREE_UPDATE_MODES()	\
	x(none)			\
	x(node)			\
	x(root)			\
	x(update)

enum __enum_closed btree_update_mode {
#define x(n)	BTREE_UPDATE_##n,
	BTREE_UPDATE_MODES()
#undef x
};

struct btree_update_node {
	struct btree			*b;
	unsigned			level;
	bool				root;
	bool				update_node_key;
	__le64				seq;
	__BKEY_PADDED(key, BKEY_BTREE_PTR_VAL_U64s_MAX);
};

typedef DARRAY_PREALLOCATED(struct btree_update_node, BTREE_UPDATE_NODES_MAX) btree_update_nodes;

/*
 * Tracks an in progress split/rewrite of a btree node and the update to the
 * parent node:
 *
 * When we split/rewrite a node, we do all the updates in memory without
 * waiting for any writes to complete - we allocate the new node(s) and update
 * the parent node, possibly recursively up to the root.
 *
 * The end result is that we have one or more new nodes being written -
 * possibly several, if there were multiple splits - and then a write (updating
 * an interior node) which will make all these new nodes visible.
 *
 * Additionally, as we split/rewrite nodes we free the old nodes - but the old
 * nodes can't be freed (their space on disk can't be reclaimed) until the
 * update to the interior node that makes the new node visible completes -
 * until then, the old nodes are still reachable on disk.
 *
 */
struct btree_update {
	struct closure			cl;
	struct bch_fs			*c;
	u64				start_time;
	unsigned long			ip_started;

	struct list_head		list;
	struct list_head		unwritten_list;

	enum btree_update_mode		mode;
	enum bch_trans_commit_flags	flags;
	unsigned			nodes_written:1;
	unsigned			took_gc_lock:1;

	enum btree_id			btree_id;
	struct bpos			node_start;
	struct bpos			node_end;
	enum btree_node_rewrite_reason	node_needed_rewrite;
	u16				node_written;
	u16				node_sectors;
	u16				node_remaining;

	unsigned			update_level_start;
	unsigned			update_level_end;

	/* size of the key that triggered split_leaf (0 if N/A) — drives
	 * the split-vs-compact decision in btree_split() so we don't loop
	 * trying to compact a leaf that can't fit the new key.
	 */
	unsigned			new_key_u64s;

	struct disk_reservation		disk_res;

	/*
	 * BTREE_UPDATE_node:
	 * The update that made the new nodes visible was a regular update to an
	 * existing interior node - @b. We can't write out the update to @b
	 * until the new nodes we created are finished writing, so we block @b
	 * from writing by putting this btree_interior update on the
	 * @b->write_blocked list with @write_blocked_list:
	 */
	struct btree			*b;
	struct list_head		write_blocked_list;

	/*
	 * We may be freeing nodes that were dirty, and thus had journal entries
	 * pinned: we need to transfer the oldest of those pins to the
	 * btree_update operation, and release it when the new node(s)
	 * are all persistent and reachable:
	 */
	struct journal_entry_pin	journal;

	/*
	 * Preallocated nodes we reserve when we start the update.
	 *
	 * b[0..consumed) have been popped by bch2_btree_node_alloc and given
	 * out to consumers (split/merge/rewrite/grow); b[consumed..nr) are
	 * still in reserve.  bch2_btree_reserve_put walks both halves and
	 * drops the as-owned intent+write refs uniformly — the consumed
	 * half also runs path/state rollback (live → NONE, drops path
	 * recurses).
	 */
	struct prealloc_nodes {
		struct btree		*b[BTREE_UPDATE_NODES_MAX];
		unsigned		nr;
		unsigned		consumed;
	}				prealloc_nodes[2];

	btree_update_nodes		old_nodes;
	btree_update_nodes		new_nodes;

	open_bucket_idx_t		open_buckets[BTREE_UPDATE_NODES_MAX *
						     BCH_REPLICAS_MAX];
	open_bucket_idx_t		nr_open_buckets;

	/* Only here to reduce stack usage on recursive splits: */
	struct keylist			parent_keys;
	/*
	 * Enough room for btree_split's keys without realloc - btree node
	 * pointers never have crc/compression info, so we only need to acount
	 * for the pointers for three keys
	 */
	u64				inline_keys[BKEY_BTREE_PTR_U64s_MAX * 3];
};

enum __enum_closed async_btree_op {
	ASYNC_BTREE_rewrite,
	ASYNC_BTREE_merge,
	ASYNC_BTREE_merge_no_read,
};

#endif /* _BCACHEFS_BTREE_INTERIOR_DEFS_H */
