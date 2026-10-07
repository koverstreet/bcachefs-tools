/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_FS_INODE_TYPES_H
#define _BCACHEFS_FS_INODE_TYPES_H

#if 0

typedef struct {
	u64			lo;
	u32			hi;
} __packed __aligned(4) u96;

#endif

typedef u64 u96;

struct bch_inode_unpacked {
	u64			bi_inum;
	u32			bi_snapshot;
	u64			bi_journal_seq;
	__le64			bi_hash_seed;
	u64			bi_size;
	u64			bi_sectors;
	u64			bi_version;
	u32			bi_flags;
	u16			bi_mode;

#define x(_name, _bits)	u##_bits _name;
	BCH_INODE_FIELDS_v3()
#undef  x
};

BITMASK(INODE_STR_HASH,	struct bch_inode_unpacked, bi_flags, 20, 24);

#include "fs/inode_opts.h"

struct bkey_inode_buf {
	struct bkey_i_inode_v3	inode;

#define x(_name, _bits)		+ 8 + _bits / 8
	u8		_pad[0 + BCH_INODE_FIELDS_v3()];
#undef  x
};

#endif /* _BCACHEFS_FS_INODE_TYPES_H */
