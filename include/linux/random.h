/*
 * include/linux/random.h
 *
 * Include file for the random number generator.
 */
#ifndef _LINUX_RANDOM_H
#define _LINUX_RANDOM_H

#include <unistd.h>
#include <sys/syscall.h>
#include <linux/bug.h>
#include <linux/kernel.h>
#include <linux/log2.h>
#include <linux/math64.h>

#ifdef SYS_getrandom
static inline int getrandom(void *buf, size_t buflen, unsigned int flags)
{
	 return syscall(SYS_getrandom, buf, buflen, flags);
}
#else
extern int urandom_fd;

static inline int getrandom(void *buf, size_t buflen, unsigned int flags)
{
	return read(urandom_fd, buf, buflen);
}
#endif

static inline void get_random_bytes(void *buf, int nbytes)
{
	BUG_ON(getrandom(buf, nbytes, 0) != nbytes);
}

#define get_random_type(type)				\
static inline type get_random_##type(void)		\
{							\
	type v;						\
							\
	get_random_bytes(&v, sizeof(v));		\
	return v;					\
}

get_random_type(int);
get_random_type(long);
get_random_type(u8);
get_random_type(u16);
get_random_type(u32);
get_random_type(u64);

static inline u32 get_random_u32_below(u32 ceil)
{
	if (ceil <= 1)
		return 0;
	for (;;) {
		if (ceil <= 1U << 8) {
			u32 mult = ceil * get_random_u8();
			if (likely(is_power_of_2(ceil) || (u8)mult >= (1U << 8) % ceil))
				return mult >> 8;
		} else if (ceil <= 1U << 16) {
			u32 mult = ceil * get_random_u16();
			if (likely(is_power_of_2(ceil) || (u16)mult >= (1U << 16) % ceil))
				return mult >> 16;
		} else {
			u64 mult = (u64)ceil * get_random_u32();
			if (likely(is_power_of_2(ceil) || (u32)mult >= -ceil % ceil))
				return mult >> 32;
		}
	}
}

static inline u32 __get_random_u32_below(u32 ceil)
{
	return get_random_u32_below(ceil);
}

static inline u64 get_random_u64_below(u64 ceil)
{
	if (ceil <= 1)
		return 0;
	if (ceil <= U32_MAX)
		return get_random_u32_below(ceil);

	for (;;) {
		u64 rand = get_random_u64();
		u64 mult = ceil * rand;
		if (likely(mult >= -ceil % ceil))
			return mul_u64_u64_shr(ceil, rand, 64);
	}
}

#endif /* _LINUX_RANDOM_H */
