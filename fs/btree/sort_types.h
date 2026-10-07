/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_BTREE_SORT_TYPES_H
#define _BCACHEFS_BTREE_SORT_TYPES_H

#include "enum_kind.h"

struct sort_iter {
	struct btree		*b;
	unsigned		used;
	unsigned		size;

	struct sort_iter_set {
		struct bkey_packed *k, *end;
	} data[];
};

struct sort_iter_stack {
	struct sort_iter	iter;
	struct sort_iter_set	sets[MAX_BSETS + 1];
};

enum __enum_closed compact_mode {
	COMPACT_LAZY,
	COMPACT_ALL,
};

#endif /* _BCACHEFS_BTREE_SORT_TYPES_H */
