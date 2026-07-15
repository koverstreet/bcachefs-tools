#ifndef __TOOLS_LINUX_BUG_H
#define __TOOLS_LINUX_BUG_H

#include <assert.h>
#include <stdio.h>
#include <linux/compiler.h>

#ifdef CONFIG_VALGRIND
#include <valgrind/memcheck.h>

#define DEBUG_MEMORY_FREED(p, len) VALGRIND_MAKE_MEM_UNDEFINED(p, len)
#endif

#define BUILD_BUG_ON_NOT_POWER_OF_2(n)			\
	BUILD_BUG_ON((n) == 0 || (((n) & ((n) - 1)) != 0))
#define BUILD_BUG_ON_ZERO(e)	(sizeof(struct { int:-!!(e); }))
#define BUILD_BUG_ON_NULL(e)	((void *)sizeof(struct { int:-!!(e); }))

#define BUILD_BUG_ON(cond)	((void)sizeof(char[1 - 2*!!(cond)]))

#define BUG()			do { fflush(stdout); assert(0); unreachable(); } while (0)
#define BUG_ON(cond)		assert(!(cond))

#define WARN(cond, fmt, ...)						\
({									\
	int __ret_warn_on = unlikely(!!(cond));				\
	if (__ret_warn_on)						\
		fprintf(stderr, "WARNING at " __FILE__ ":%d: " fmt "\n",\
			__LINE__, ##__VA_ARGS__);			\
	__ret_warn_on;							\
})

#define __WARN()							\
do {									\
	fprintf(stderr, "WARNING at " __FILE__ ":%d\n", __LINE__);	\
} while (0)

#define WARN_ON(cond) ({						\
	int __ret_warn_on = unlikely(!!(cond));				\
	if (__ret_warn_on)						\
		__WARN();						\
	__ret_warn_on;							\
})

#define WARN_ONCE(cond, fmt, ...)					\
({									\
	static bool __warned;						\
	int __ret_warn_on = unlikely(!!(cond));				\
	if (__ret_warn_on && !__warned) {				\
		__warned = true;					\
		__WARN();						\
	}								\
	__ret_warn_on;							\
})

#define WARN_ON_ONCE(cond) ({						\
	static bool __warned;						\
	int __ret_warn_on = unlikely(!!(cond));				\
	if (__ret_warn_on && !__warned) {				\
		__warned = true;					\
		__WARN();						\
	}								\
	__ret_warn_on;							\
})

#endif /* __TOOLS_LINUX_BUG_H */
