/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_BTREE_READ_TYPES_H
#define _BCACHEFS_BTREE_READ_TYPES_H

struct bch_fs;

struct btree;

struct btree_read_bio {
	struct bch_fs		*c;
	struct bch_dev		*ca;	/* stashed at submit; see bch_write_bio */
	struct btree		*b;
	u64			start_time;
	unsigned		idx:7;
#ifdef CONFIG_BCACHEFS_ASYNC_OBJECT_LISTS
	unsigned		list_idx;
#endif
	struct extent_ptr_decoded	pick;
	struct work_struct	work;
	struct bio		bio;
};

typedef void (*btree_node_scrub_report_fn)(void *priv, unsigned dev, bool good);

#endif /* _BCACHEFS_BTREE_READ_TYPES_H */
