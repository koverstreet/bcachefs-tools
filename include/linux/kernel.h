#ifndef __TOOLS_LINUX_KERNEL_H
#define __TOOLS_LINUX_KERNEL_H

#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <limits.h>

#include <linux/bug.h>
#include <linux/cleanup.h>
#include <linux/compiler.h>

#define BITS_PER_LONG	__BITS_PER_LONG

struct page;
struct kmem_cache;

typedef unsigned short		umode_t;

typedef unsigned gfp_t;

#define GFP_ATOMIC	0
#define GFP_NOIO	0
#define GFP_NOWAIT	0
#define __GFP_RECLAIM	0
#define __GFP_DIRECT_RECLAIM 0
#define __GFP_IO	0
#define __GFP_NOWARN	0
#define __GFP_NORETRY	0
#define __GFP_NOMEMALLOC 0
#define __GFP_NOFAIL	0
#define __GFP_ACCOUNT	0
#define __GFP_RECLAIMABLE 0
#define __GFP_ZERO	1
#define __GFP_SKIP_ZERO	0
#define GFP_KERNEL	2

#define PAGE_ALLOC_COSTLY_ORDER	6

typedef __u64 u64;
typedef __s64 s64;
typedef __u32 u32;
typedef __u16 u16;
typedef __u8  u8;

typedef unsigned int		uint;

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

#define U32_MAX		((u32)~0U)

#define __must_be_array(a)	BUILD_BUG_ON_ZERO(__same_type((a), &(a)[0]))
#define ARRAY_SIZE(arr) (sizeof(arr) / sizeof((arr)[0]) + __must_be_array(arr))

#ifndef offsetof
#define offsetof(TYPE, MEMBER) ((size_t) &((TYPE *)0)->MEMBER)
#endif

#ifndef container_of
/**
 * container_of - cast a member of a structure out to the containing structure
 * @ptr:	the pointer to the member.
 * @type:	the type of the container struct this is embedded in.
 * @member:	the name of the member within the struct.
 *
 */
#define container_of(ptr, type, member) ({			\
	const typeof(((type *)0)->member) * __mptr = (ptr);	\
	(type *)((char *)__mptr - offsetof(type, member)); })
#endif

#ifndef __struct_group
#define __struct_group(TAG, NAME, ATTRS, MEMBERS...) \
	union { \
		struct { MEMBERS } ATTRS; \
		struct TAG { MEMBERS } ATTRS NAME; \
	}
#endif

#define _RET_IP_		(unsigned long)__builtin_return_address(0)
#define _THIS_IP_  ({ __label__ __here; __here: (unsigned long)&&__here; })

#define panic(fmt, ...)					\
do {							\
	printf(fmt, ##__VA_ARGS__);			\
	BUG();						\
} while (0)

/* The hash is always the low bits of hash_len */
#if __BYTE_ORDER__ == __ORDER_LITTLE_ENDIAN__
 #define HASH_LEN_DECLARE u32 hash; u32 len
#else
 #define HASH_LEN_DECLARE u32 len; u32 hash
#endif

struct qstr {
	union {
		struct {
			HASH_LEN_DECLARE;
		};
		u64 hash_len;
	};
	const unsigned char *name;
};

typedef u64 sector_t;

#define DIV_ROUND_UP(n,d) (((n) + (d) - 1) / (d))

static inline size_t copy_to_user(void *dst, void *src, size_t len)
{
	memcpy(dst, src, len);
	return 0;
}

#endif
