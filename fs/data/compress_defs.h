/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_DATA_COMPRESS_DEFS_H
#define _BCACHEFS_DATA_COMPRESS_DEFS_H

#include "enum_kind.h"

union bch_compression_opt {
	u8 value;
	struct {
#if defined(__LITTLE_ENDIAN_BITFIELD)
		u8 type:4, level:4;
#elif defined(__BIG_ENDIAN_BITFIELD)
		u8 level:4, type:4;
#endif
	};
};

/*
 * Bounce buffer for the encode/decode paths: either a direct mapping of a
 * bio's pages (BB_none/BB_vmap - no copy) or a private allocation the caller
 * can scribble on without touching the bio. Exported because the write path
 * assembles its own decode sequence out of these.
 */
struct bbuf {
	struct bch_fs	*c;
	void		*b;
	enum __enum_closed bbuf_type {
		BB_none,
		BB_vmap,
		BB_kmalloc,
		BB_mempool,
	}		type;
	int		rw;
};

#endif /* _BCACHEFS_DATA_COMPRESS_DEFS_H */
