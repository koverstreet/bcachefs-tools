/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_BTREE_WRITE_BUFFER_DEFS_H
#define _BCACHEFS_BTREE_WRITE_BUFFER_DEFS_H

struct btree_trans;

struct wb_maybe_flush {
	struct bkey_buf	last_flushed;
	u32		flushed_commit_count;
	u64		nr_flushes;
	u64		nr_done;
	bool		seen_error;
};

struct journal_keys_to_wb_btree {
	struct btree_write_buffer_keys	*wb;	/* NULL: not yet acquired */
	size_t				room;
};

struct journal_keys_to_wb {
	u64				seq;
#ifdef CONFIG_BCACHEFS_TESTS
	bool				test_wb_pin_armed;
#endif
	struct journal_keys_to_wb_btree	per_btree[BCH_WB_BTREE_NR];
};

#endif /* _BCACHEFS_BTREE_WRITE_BUFFER_DEFS_H */
