#ifndef _LINUX_KTHREAD_H
#define _LINUX_KTHREAD_H

/* Simple interface for creating and stopping kernel threads without mess. */
#include <stdio.h>
#include <linux/err.h>
#include <linux/lockdep.h>
#include <linux/sched.h>
#include <linux/spinlock.h>

/* os/linux_user/kthread.rs */
struct task_struct *kthread_create_name(int (*threadfn)(void *data),
					void *data, const char *name);
int kthread_stop(struct task_struct *k);
bool kthread_should_stop(void);

/*
 * A macro, as in the kernel: Rust can't define a variadic function. The name
 * is cut to TASK_COMM_LEN there, as the kernel does.
 */
#define kthread_create(threadfn, data, namefmt, ...)			   \
({									   \
	char __name[64];						   \
	snprintf(__name, sizeof(__name), namefmt, ## __VA_ARGS__);	   \
	kthread_create_name(threadfn, data, __name);			   \
})

/**
 * kthread_run - create and wake a thread.
 * @threadfn: the function to run until signal_pending(current).
 * @data: data ptr for @threadfn.
 * @namefmt: printf-style name for the thread.
 *
 * Description: Convenient wrapper for kthread_create() followed by
 * wake_up_process().  Returns the kthread or ERR_PTR(-ENOMEM).
 */
#define kthread_run(threadfn, data, namefmt, ...)			   \
({									   \
	struct task_struct *__k						   \
		= kthread_create(threadfn, data, namefmt, ## __VA_ARGS__); \
	if (!IS_ERR(__k))						   \
		wake_up_process(__k);					   \
	__k;								   \
})

#endif /* _LINUX_KTHREAD_H */
