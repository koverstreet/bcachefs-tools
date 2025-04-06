/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _LINUX_STRING_HELPERS_H_
#define _LINUX_STRING_HELPERS_H_

#include <linux/ctype.h>
#include <linux/string.h>
#include <linux/types.h>


/* Descriptions of the types of units to
 * print in */
enum string_size_units {
	STRING_UNITS_10,	/* use powers of 10^3 (standard SI) */
	STRING_UNITS_2,		/* use binary powers of 2^10 */
};

int string_get_size(u64 size, u64 blk_size, enum string_size_units units,
		    char *buf, int len);

static inline void memcpy_and_pad(void *dest, size_t dest_len, const void *src,
				  size_t count, int pad)
{
	if (dest_len > count) {
		memcpy(dest, src, count);
		memset(dest + count, pad,  dest_len - count);
	} else {
		memcpy(dest, src, dest_len);
	}
}

#endif
