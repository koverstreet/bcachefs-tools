#ifndef __TOOLS_LINUX_BUG_H
#define __TOOLS_LINUX_BUG_H

#include <assert.h>
#include <stdio.h>
#include <linux/compiler.h>

#ifdef CONFIG_VALGRIND
#include <valgrind/memcheck.h>

#endif

#define BUILD_BUG_ON_ZERO(e)	(sizeof(struct { int:-!!(e); }))

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

#endif /* __TOOLS_LINUX_BUG_H */
