
#include <stdio.h>
#include <unistd.h>

#include <linux/blkdev.h>
#include <linux/kthread.h>
#include <linux/list.h>
#include <linux/mm.h>
#include <linux/mutex.h>
#include <linux/percpu.h>
#include <linux/shrinker.h>

#include "tools-util.h"

static LIST_HEAD(shrinker_list);
static DEFINE_MUTEX(shrinker_lock);

void shrinker_free(struct shrinker *s)
{
	if (!s)
		return;

	if (s->list.next) {
		mutex_lock(&shrinker_lock);
		list_del(&s->list);
		mutex_unlock(&shrinker_lock);
	}
	free(s);
}

struct shrinker *shrinker_alloc(unsigned int flags, const char *fmt, ...)
{
	return calloc(sizeof(struct shrinker), 1);
}

int shrinker_register(struct shrinker *shrinker)
{
	mutex_lock(&shrinker_lock);
	list_add_tail(&shrinker->list, &shrinker_list);
	mutex_unlock(&shrinker_lock);
	return 0;
}

static void run_shrinkers_allocation_failed(gfp_t gfp_mask)
{
	struct shrinker *shrinker;

	mutex_lock(&shrinker_lock);
	list_for_each_entry(shrinker, &shrinker_list, list) {
		struct shrink_control sc = { .gfp_mask	= gfp_mask, };

		unsigned long have = shrinker->count_objects(shrinker, &sc);

		sc.nr_to_scan = have / 8;

		shrinker->scan_objects(shrinker, &sc);
	}
	mutex_unlock(&shrinker_lock);
}

void run_shrinkers(gfp_t gfp_mask, bool allocation_failed)
{
	struct shrinker *shrinker;
	struct sysinfo info;
	s64 want_shrink;

	if (!(gfp_mask & GFP_KERNEL))
		return;

	/* Fast out if there are no shrinkers to run. */
	if (list_empty(&shrinker_list))
		return;

	if (allocation_failed) {
		run_shrinkers_allocation_failed(gfp_mask);
		return;
	}

	si_meminfo(&info);

	/* Aim for 6% of physical RAM free without anything in swap */
	want_shrink = (info.totalram >> 4) - info.freeram
			+ info.totalswap - info.freeswap;
	if (want_shrink <= 0)
		return;

	mutex_lock(&shrinker_lock);
	list_for_each_entry(shrinker, &shrinker_list, list) {
		struct shrink_control sc = {
			.gfp_mask	= gfp_mask,
			.nr_to_scan	= want_shrink >> PAGE_SHIFT
		};

		shrinker->scan_objects(shrinker, &sc);
	}
	mutex_unlock(&shrinker_lock);
}

static int shrinker_thread(void *arg)
{
	while (!kthread_should_stop()) {
		struct timespec to;
		int v;

		clock_gettime(CLOCK_MONOTONIC, &to);
		to.tv_sec += 1;
		__set_current_state(TASK_INTERRUPTIBLE);
		errno = 0;
		while ((v = READ_ONCE(current->state)) != TASK_RUNNING &&
		       errno != ETIMEDOUT)
			futex(&current->state, FUTEX_WAIT_BITSET|FUTEX_PRIVATE_FLAG,
			      v, &to, NULL, (uint32_t)~0);
		if (kthread_should_stop())
			break;
		if (v != TASK_RUNNING)
			__set_current_state(TASK_RUNNING);
		run_shrinkers(GFP_KERNEL, false);
	}

	return 0;
}

struct task_struct *shrinker_task;
unsigned long _totalram_pages;

void linux_shrinkers_init(void)
{
	rcu_init();
	rcu_register_thread();
	bch_percpu_thread_init();

	blkdev_init();

	struct sysinfo info;
	si_meminfo(&info);
	_totalram_pages = info.totalram >> PAGE_SHIFT;

	shrinker_task = kthread_run(shrinker_thread, NULL, "shrinkers");
	BUG_ON(IS_ERR(shrinker_task));
}

#if 0
/*
 * We seem to be hitting a rare segfault when shutting down the shrinker thread.
 * Disabling this is going to cause some harmless warnings about memory leaks:
 */
__attribute__((destructor(103)))
static void shrinker_thread_exit(void)
{
	int ret = kthread_stop(shrinker_task);
	BUG_ON(ret);

	shrinker_task = NULL;
}
#endif
