#ifndef __TOOLS_LINUX_KERNEL_H
#define __TOOLS_LINUX_KERNEL_H

/* What linux/unicode/ uses of the kernel's headers */

#include <errno.h>
#include <stddef.h>
#include <stdlib.h>
#include <string.h>
#include <linux/types.h>

#define __ARG_PLACEHOLDER_1 0,
#define __take_second_arg(__ignored, val, ...) val

#define __is_defined(x)			___is_defined(x)
#define ___is_defined(val)		____is_defined(__ARG_PLACEHOLDER_##val)
#define ____is_defined(arg1_or_junk)	__take_second_arg(arg1_or_junk 1, 0)

/*
 * IS_MODULE(CONFIG_FOO) evaluates to 1 if CONFIG_FOO is set to 'm', 0
 * otherwise.
 */
#define IS_MODULE(option) __is_defined(option##_MODULE)

#define __same_type(a, b)	__builtin_types_compatible_p(typeof(a), typeof(b))
#define BUILD_BUG_ON_ZERO(e)	(sizeof(struct { int:-!!(e); }))
#define __must_be_array(a)	BUILD_BUG_ON_ZERO(__same_type((a), &(a)[0]))
#define ARRAY_SIZE(arr) (sizeof(arr) / sizeof((arr)[0]) + __must_be_array(arr))

/* The hash is always the low bits of hash_len */
#if __BYTE_ORDER__ == __ORDER_LITTLE_ENDIAN__
 #define HASH_LEN_DECLARE __u32 hash; __u32 len
#else
 #define HASH_LEN_DECLARE __u32 len; __u32 hash
#endif

struct qstr {
	union {
		struct {
			HASH_LEN_DECLARE;
		};
		__u64 hash_len;
	};
	const unsigned char *name;
};

#endif
