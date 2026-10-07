/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_BTREE_BKEY_BUF_TYPES_H
#define _BCACHEFS_BTREE_BKEY_BUF_TYPES_H

struct bkey_buf {
	struct bkey_i	*k;
	u64		onstack[12];
};

#endif /* _BCACHEFS_BTREE_BKEY_BUF_TYPES_H */
