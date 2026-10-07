/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_DEBUG_DEBUG_TYPES_H
#define _BCACHEFS_DEBUG_DEBUG_TYPES_H

struct bio;

struct btree;

struct bch_fs;

#ifdef CONFIG_DEBUG_FS

struct dump_iter {
	struct bch_fs		*c;
	struct async_obj_list	*list;
	enum btree_id		id;
	unsigned		level;
	struct bpos		from;
	struct bpos		prev_node;
	u64			iter;

	struct printbuf		buf;

	char __user		*ubuf;	/* destination user buffer */
	size_t			size;	/* size of requested read */
	ssize_t			ret;	/* bytes read so far */
};

#endif

#endif /* _BCACHEFS_DEBUG_DEBUG_TYPES_H */
