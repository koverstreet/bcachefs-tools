/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_BTREE_WRITE_TYPES_H
#define _BCACHEFS_BTREE_WRITE_TYPES_H

#include "enum_kind.h"

struct btree_write_bio {
	struct work_struct	work;
	__BKEY_PADDED(key, BKEY_BTREE_PTR_VAL_U64s_MAX);
	void			*data;
	unsigned		data_bytes;
	unsigned		sector_offset;
	u64			start_time;
#ifdef CONFIG_BCACHEFS_ASYNC_OBJECT_LISTS
	unsigned		list_idx;
#endif
	struct bch_write_bio	wbio;
};

enum __enum_closed btree_write_flags {
	__BTREE_WRITE_only_if_need = BTREE_WRITE_TYPE_BITS,
	__BTREE_WRITE_already_started,
};

#endif /* _BCACHEFS_BTREE_WRITE_TYPES_H */
