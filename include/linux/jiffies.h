#ifndef _LINUX_JIFFIES_H
#define _LINUX_JIFFIES_H

#include <time.h>
#include <linux/kernel.h>
#include <linux/time64.h>
#include <linux/typecheck.h>
#include <linux/types.h>

#define time_after(a,b)		\
	(typecheck(unsigned long, a) && \
	 typecheck(unsigned long, b) && \
	 ((long)((b) - (a)) < 0))
#define time_before(a,b)	time_after(b,a)

#define time_after_eq(a,b)	\
	(typecheck(unsigned long, a) && \
	 typecheck(unsigned long, b) && \
	 ((long)((a) - (b)) >= 0))
#define time_before_eq(a,b)	time_after_eq(b,a)

#define time_in_range(a,b,c) \
	(time_after_eq(a,b) && \
	 time_before_eq(a,c))

#define time_in_range_open(a,b,c) \
	(time_after_eq(a,b) && \
	 time_before(a,c))

#define time_after64(a,b)	\
	(typecheck(__u64, a) &&	\
	 typecheck(__u64, b) && \
	 ((__s64)((b) - (a)) < 0))
#define time_before64(a,b)	time_after64(b,a)

#define time_after_eq64(a,b)	\
	(typecheck(__u64, a) && \
	 typecheck(__u64, b) && \
	 ((__s64)((a) - (b)) >= 0))
#define time_before_eq64(a,b)	time_after_eq64(b,a)

#define time_in_range64(a, b, c) \
	(time_after_eq64(a, b) && \
	 time_before_eq64(a, c))

#define time_is_before_jiffies(a) time_after(jiffies, a)

#define HZ		1000

static inline u64 jiffies_to_nsecs(const unsigned long j)
{
	return (u64)j * NSEC_PER_MSEC;
}

static inline unsigned jiffies_to_msecs(const unsigned long j)
{
	return j;
}

static inline unsigned long msecs_to_jiffies(const unsigned int m)
{
	return m;
}

static inline unsigned long nsecs_to_jiffies(u64 n)
{
	return n / NSEC_PER_MSEC;
}

static inline u64 sched_clock(void)
{
	struct timespec ts;

	clock_gettime(CLOCK_MONOTONIC_COARSE, &ts);

	return ((s64) ts.tv_sec * NSEC_PER_SEC) + ts.tv_nsec;
}

static inline u64 local_clock(void)
{
	return sched_clock();
}

static inline u64 ktime_get_ns(void)
{
	return sched_clock();
}

#define jiffies			nsecs_to_jiffies(sched_clock())

#endif
