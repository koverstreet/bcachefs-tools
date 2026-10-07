/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_UTIL_SIPHASH_TYPES_H
#define _BCACHEFS_UTIL_SIPHASH_TYPES_H

#define SIPHASH_BLOCK_LENGTH	 8

typedef struct _SIPHASH_CTX {
	u64		v[4];
	u8		buf[SIPHASH_BLOCK_LENGTH];
	u32		bytes;
} SIPHASH_CTX;

typedef struct {
	__le64		k0;
	__le64		k1;
} SIPHASH_KEY;

#endif /* _BCACHEFS_UTIL_SIPHASH_TYPES_H */
