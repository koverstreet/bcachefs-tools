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
#include <linux/percpu.h>
#include <linux/preempt.h>
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

/*
 * bch2_alloc_percpu_init(): run init() on every per-cpu instance of a
 * dynamically-allocated percpu variable.
 *
 * Kernel: iterates for_each_possible_cpu and calls init() once per cpu
 * at allocation time.
 *
 * Userspace: alloc_percpu() returns an offset into a per-thread chunk;
 * threads come and go dynamically, so init() must also be called for
 * each thread that's spawned later. The userspace shim stashes the
 * (init, ctx) pair alongside the percpu variable so it can call init()
 * for every existing chunk now and every future chunk at thread
 * creation time.
 *
 * The init function takes (per-instance ptr, caller ctx, cpu id).
 */
#ifdef __KERNEL__
#define bch2_alloc_percpu_init(_pcv, _init, _ctx)			\
do {									\
	int _cpu;							\
	for_each_possible_cpu(_cpu)					\
		(_init)(per_cpu_ptr((_pcv), _cpu), (_ctx), _cpu);	\
} while (0)
#else
void __bch2_alloc_percpu_init(void *pcv,
			      void (*init)(void *, void *, unsigned),
			      void *ctx);
#define bch2_alloc_percpu_init(_pcv, _init, _ctx)			\
	__bch2_alloc_percpu_init((_pcv),				\
		(void (*)(void *, void *, unsigned))(_init), (_ctx))
#endif

#endif /* _BCACHEFS_UTIL_H */
