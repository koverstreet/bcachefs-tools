/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _LINUX_UNALIGNED_H
#define _LINUX_UNALIGNED_H

#include <string.h>
#include <linux/kernel.h>

/* Little-endian loads and stores at any alignment: xxhash's and chacha's */

static inline u32 get_unaligned_le32(const void *p)
{
	u32 v;

	memcpy(&v, p, sizeof(v));
#if __BYTE_ORDER__ == __ORDER_BIG_ENDIAN__
	v = __builtin_bswap32(v);
#endif
	return v;
}

static inline u64 get_unaligned_le64(const void *p)
{
	u64 v;

	memcpy(&v, p, sizeof(v));
#if __BYTE_ORDER__ == __ORDER_BIG_ENDIAN__
	v = __builtin_bswap64(v);
#endif
	return v;
}

static inline void put_unaligned_le32(u32 v, void *p)
{
#if __BYTE_ORDER__ == __ORDER_BIG_ENDIAN__
	v = __builtin_bswap32(v);
#endif
	memcpy(p, &v, sizeof(v));
}

#endif /* _LINUX_UNALIGNED_H */
