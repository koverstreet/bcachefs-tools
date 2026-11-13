#ifndef __TOOLS_LINUX_WORKQUEUE_H
#define __TOOLS_LINUX_WORKQUEUE_H

#include <stdio.h>
#include <linux/atomic.h>
#include <linux/bitops.h>
#include <linux/list.h>

/* What closure.c uses of os/linux_user/workqueue.rs */

struct task_struct;
struct workqueue_struct;
struct work_struct;
typedef void (*work_func_t)(struct work_struct *work);

/*
 * @entry is unused here, but keeps @func where the kernel has it: a closure's
 * fn is at the same offset (closure_queue()).
 */
struct work_struct {
	atomic_long_t data;
	struct list_head entry;
	work_func_t func;
};

#define INIT_WORK(_work, _func)					\
do {								\
	(_work)->data.counter = 0;				\
	INIT_LIST_HEAD(&(_work)->entry);			\
	(_work)->func = (_func);				\
} while (0)

/* Its delay is in the workqueues' list of delayed work, not here */
struct delayed_work {
	struct work_struct work;
};

extern bool queue_work(struct workqueue_struct *wq,
		       struct work_struct *work);

#endif /* __TOOLS_LINUX_WORKQUEUE_H */
