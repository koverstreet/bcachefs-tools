#ifndef __TOOLS_LINUX_WORKQUEUE_H
#define __TOOLS_LINUX_WORKQUEUE_H

#include <stdio.h>
#include <linux/atomic.h>
#include <linux/bitops.h>
#include <linux/list.h>

/* os/linux_user/workqueue.rs's work items, as the kernel lays them out */

struct task_struct;
struct workqueue_struct;
struct work_struct;
typedef void (*work_func_t)(struct work_struct *work);

/* @entry is unused here: the layout is the kernel's */
struct work_struct {
	atomic_long_t data;
	struct list_head entry;
	work_func_t func;
};

/* Its delay is in the workqueues' list of delayed work, not here */
struct delayed_work {
	struct work_struct work;
};

#endif /* __TOOLS_LINUX_WORKQUEUE_H */
