#ifndef _TOOLS_LINUX_STRING_H_
#define _TOOLS_LINUX_STRING_H_

#include <stdlib.h>
#include <string.h>
#include <linux/string_helpers.h>
#include <linux/types.h>	/* for size_t */

extern size_t strlcpy(char *dest, const char *src, size_t size);
extern ssize_t strscpy(char *dest, const char *src, size_t count);
extern char *strim(char *);
extern void memzero_explicit(void *, size_t);
int match_string(const char * const *, size_t, const char *);
extern void * memscan(void *,int, size_t);

#define kstrndup(s, n, gfp)		strndup(s, n)
#define kstrdup(s, gfp)			strdup(s)

#define strtomem_pad(dest, src, pad)	do {				\
	const size_t _dest_len = ARRAY_SIZE(dest);			\
	const size_t _src_len = __builtin_object_size(src, 1);		\
									\
	memcpy_and_pad(dest, _dest_len, src,				\
		       strnlen(src, min(_src_len, _dest_len)), pad);	\
} while (0)

#endif /* _LINUX_STRING_H_ */
