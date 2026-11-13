/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_UTIL_H
#define _BCACHEFS_UTIL_H

#include <linux/bio.h>
#include <linux/blkdev.h>
#ifdef __KERNEL__
#include <linux/capability.h>
#endif
#include <linux/errno.h>
#include <linux/freezer.h>
#include <linux/kernel.h>
#include <linux/sched/clock.h>
#include <linux/llist.h>
#include <linux/log2.h>
#include <linux/random.h>
#include <linux/ratelimit.h>
#include <linux/slab.h>
#include <linux/sort.h>
#include <linux/version.h>
#include <linux/vmalloc.h>
#include <linux/workqueue.h>

#include "vendor/closure.h"
#include <linux/math64.h>
#include <linux/types.h>
#include <linux/local_lock.h>

#ifdef __KERNEL__
#include <linux/cpumask.h>
#include <linux/numa.h>
#include <linux/percpu.h>
#include <linux/preempt.h>
#else
#include <uuid/uuid.h>
#endif
#include <linux/uuid.h>
#include <linux/sched/mm.h>

#include "util/shim.h"

/*
 * 7.1 introduced bdev_rot() and removed bdev_nonrot(). Shim bdev_rot for
 * older kernels and userspace so call sites can always use bdev_rot.
 */
#if !defined(__KERNEL__) || LINUX_VERSION_CODE < KERNEL_VERSION(7,0,0)
static inline bool bdev_rot(struct block_device *bdev)
{
	return !bdev_nonrot(bdev);
}
#endif

#define try(_do)							\
do {									\
	typeof(_do) _ret = (_do);					\
	if (unlikely(_ret))						\
		return _ret;						\
} while (0)

#define errptr_try(_do)							\
({									\
	typeof(_do) _ret = (_do);					\
	if (IS_ERR(_ret))						\
		return PTR_ERR(_ret);					\
	_ret;								\
})

#endif /* _BCACHEFS_UTIL_H */
