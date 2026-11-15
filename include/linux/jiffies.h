#ifndef _LINUX_JIFFIES_H
#define _LINUX_JIFFIES_H

#include <time.h>
#include <linux/kernel.h>
#include <linux/time64.h>
#include <linux/types.h>

#define HZ		1000

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
