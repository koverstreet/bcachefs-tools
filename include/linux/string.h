#ifndef _TOOLS_LINUX_STRING_H_
#define _TOOLS_LINUX_STRING_H_

#include <stdlib.h>
#include <string.h>
#include <linux/types.h>	/* for size_t */


#define kstrndup(s, n, gfp)		strndup(s, n)
#define kstrdup(s, gfp)			strdup(s)

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

#endif /* _LINUX_STRING_H_ */
