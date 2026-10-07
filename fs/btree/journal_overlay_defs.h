/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_BTREE_JOURNAL_OVERLAY_DEFS_H
#define _BCACHEFS_BTREE_JOURNAL_OVERLAY_DEFS_H

struct journal_iter {
	struct list_head	list;
	enum btree_id		btree_id;
	unsigned		level;
	size_t			idx;
	struct journal_keys	*keys;
};

/*
 * Iterate over keys in the btree, with keys from the journal overlaid on top:
 */

struct btree_and_journal_iter {
	struct btree_trans	*trans;
	struct btree		*b;
	struct btree_node_iter	node_iter;
	struct bkey		unpacked;

	struct journal_iter	journal;
	struct bpos		pos;
	bool			at_end;
	bool			prefetch;
	bool			fail_if_too_many_whiteouts;
};

#endif /* _BCACHEFS_BTREE_JOURNAL_OVERLAY_DEFS_H */
