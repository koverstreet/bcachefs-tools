#ifndef _LINUX_PRANDOM_H
#define _LINUX_PRANDOM_H

#include <linux/random.h>

static inline void prandom_bytes(void *buf, int nbytes)
{
	return get_random_bytes(buf, nbytes);
}

#define prandom_type(type)				\
static inline type prandom_##type(void)			\
{							\
	type v;						\
							\
	prandom_bytes(&v, sizeof(v));			\
	return v;					\
}

prandom_type(int);
prandom_type(long);
prandom_type(u32);
prandom_type(u64);
#undef prandom_type

static inline u32 prandom_u32_max(u32 max)
{
	return prandom_u32() % max;

}

#endif /* _LINUX_PRANDOM_H */

