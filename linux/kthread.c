#include <pthread.h>
#include <stdlib.h>
#include <string.h>

#include <linux/bitops.h>
#include <linux/kthread.h>
#include <linux/rcupdate.h>
#include <linux/sched.h>

#include "tools-util.h"

enum KTHREAD_BITS {
	KTHREAD_IS_PER_CPU = 0,
	KTHREAD_SHOULD_STOP,
	KTHREAD_SHOULD_PARK,
	KTHREAD_IS_PARKED,
};

static void *kthread_start_fn(void *data)
{
	rcu_register_thread();

	current = data;
	schedule();
	current->thread_fn(current->thread_data);

	complete(&current->exited);
	put_task_struct(current);
	rcu_unregister_thread();
	return NULL;
}

/**
 * kthread_create_on_node - create a kthread.
 * @threadfn: the function to run until signal_pending(current).
 * @data: data ptr for @threadfn.
 * @node: task and thread structures for the thread are allocated on this node
 * @namefmt: printf-style name for the thread.
 *
 * Description: This helper function creates and names a kernel
 * thread.  The thread will be stopped: use wake_up_process() to start
 * it.  See also kthread_run().  The new thread has SCHED_NORMAL policy and
 * is affine to all CPUs.
 *
 * If thread is going to be bound on a particular cpu, give its node
 * in @node, to get NUMA affinity for kthread stack, or else give NUMA_NO_NODE.
 * When woken, the thread will run @threadfn() with @data as its
 * argument. @threadfn() can either call do_exit() directly if it is a
 * standalone thread for which no one will call kthread_stop(), or
 * return when 'kthread_should_stop()' is true (which means
 * kthread_stop() has been called).  The return value should be zero
 * or a negative error number; it will be passed to kthread_stop().
 *
 * Returns a task_struct or ERR_PTR(-ENOMEM) or ERR_PTR(-EINTR).
 */
struct task_struct *kthread_create(int (*thread_fn)(void *data),
				   void *thread_data,
				   const char namefmt[], ...)
{
	va_list args;
	struct task_struct *p = malloc(sizeof(*p));
	int ret;

	memset(p, 0, sizeof(*p));

	va_start(args, namefmt);
	vsnprintf(p->comm, sizeof(p->comm), namefmt, args);
	va_end(args);

	p->flags	|= PF_KTHREAD;
	p->thread_fn	= thread_fn;
	p->thread_data	= thread_data;
	p->state	= TASK_UNINTERRUPTIBLE;
	atomic_set(&p->usage, 1);
	init_completion(&p->exited);

	pthread_attr_t attr;
	pthread_attr_init(&attr);
	pthread_attr_setstacksize(&attr, 32 << 10);

	ret = pthread_create(&p->thread, &attr, kthread_start_fn, p);
	if (ret)
		return ERR_PTR(-ret);
	pthread_setname_np(p->thread, p->comm);
	return p;
}

/**
 * kthread_should_stop - should this kthread return now?
 *
 * When someone calls kthread_stop() on your kthread, it will be woken
 * and this will return true.  You should then return, and your return
 * value will be passed through to kthread_stop().
 */
bool kthread_should_stop(void)
{
	return test_bit(KTHREAD_SHOULD_STOP, &current->kthread_flags);
}

/**
 * kthread_stop - stop a thread created by kthread_create().
 * @k: thread created by kthread_create().
 *
 * Sets kthread_should_stop() for @k to return true, wakes it, and
 * waits for it to exit. This can also be called after kthread_create()
 * instead of calling wake_up_process(): the thread will exit without
 * calling threadfn().
 *
 * If threadfn() may call do_exit() itself, the caller must ensure
 * task_struct can't go away.
 *
 * Returns the result of threadfn(), or %-EINTR if wake_up_process()
 * was never called.
 */
int kthread_stop(struct task_struct *p)
{
	get_task_struct(p);

	set_bit(KTHREAD_SHOULD_STOP, &p->kthread_flags);
	wake_up_process(p);
	wait_for_completion(&p->exited);

	put_task_struct(p);

	return 0;
}
