#ifndef __TOOLS_LINUX_WORKQUEUE_H
#define __TOOLS_LINUX_WORKQUEUE_H

#include <linux/list.h>
#include <linux/timer.h>

struct task_struct;
struct workqueue_struct;
struct work_struct;
typedef void (*work_func_t)(struct work_struct *work);
void delayed_work_timer_fn(struct timer_list *);

#define work_data_bits(work) ((unsigned long *)(&(work)->data))

#if 0
enum {
	//WORK_STRUCT_PENDING_BIT	= 0,	/* work item is pending execution */
	//WORK_STRUCT_DELAYED_BIT	= 1,	/* work item is delayed */
	//
	//WORK_STRUCT_PENDING	= 1 << WORK_STRUCT_PENDING_BIT,
	//WORK_STRUCT_DELAYED	= 1 << WORK_STRUCT_DELAYED_BIT,
};
#endif

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

struct delayed_work {
	struct work_struct work;
	struct timer_list timer;
	struct workqueue_struct *wq;
};

#define INIT_DELAYED_WORK(_work, _func)					\
	do {								\
		INIT_WORK(&(_work)->work, (_func));			\
		timer_setup(&(_work)->timer, delayed_work_timer_fn, 0);	\
	} while (0)

static inline struct delayed_work *to_delayed_work(struct work_struct *work)
{
	return container_of(work, struct delayed_work, work);
}

enum {
	WQ_UNBOUND		= 1 << 1, /* not bound to any cpu */
	WQ_FREEZABLE		= 1 << 2, /* freeze during suspend */
	WQ_MEM_RECLAIM		= 1 << 3, /* may be used for memory reclaim */
	WQ_HIGHPRI		= 1 << 4, /* high priority */
	WQ_CPU_INTENSIVE	= 1 << 5, /* cpu intensive workqueue */
	WQ_SYSFS		= 1 << 6, /* visible in sysfs, see wq_sysfs_register() */

	/*
	 * Per-cpu workqueues are generally preferred because they tend to
	 * show better performance thanks to cache locality.  Per-cpu
	 * workqueues exclude the scheduler from choosing the CPU to
	 * execute the worker threads, which has an unfortunate side effect
	 * of increasing power consumption.
	 *
	 * The scheduler considers a CPU idle if it doesn't have any task
	 * to execute and tries to keep idle cores idle to conserve power;
	 * however, for example, a per-cpu work item scheduled from an
	 * interrupt handler on an idle CPU will force the scheduler to
	 * execute the work item on that CPU breaking the idleness, which in
	 * turn may lead to more scheduling choices which are sub-optimal
	 * in terms of power consumption.
	 *
	 * Workqueues marked with WQ_POWER_EFFICIENT are per-cpu by default
	 * but become unbound if workqueue.power_efficient kernel param is
	 * specified.  Per-cpu workqueues which are identified to
	 * contribute significantly to power-consumption are identified and
	 * marked with this flag and enabling the power_efficient mode
	 * leads to noticeable power saving at the cost of small
	 * performance disadvantage.
	 *
	 * http://thread.gmane.org/gmane.linux.kernel/1480396
	 */
	WQ_POWER_EFFICIENT	= 1 << 7,

	__WQ_DRAINING		= 1 << 16, /* internal: workqueue is draining */
	__WQ_ORDERED		= 1 << 17, /* internal: workqueue is ordered */
	__WQ_LEGACY		= 1 << 18, /* internal: create*_workqueue() */

	WQ_MAX_ACTIVE		= 512,	  /* I like 512, better ideas? */
	WQ_MAX_UNBOUND_PER_CPU	= 4,	  /* 4 * #cpus for unbound wq */
	WQ_DFL_ACTIVE		= WQ_MAX_ACTIVE / 2,
};

/* unbound wq's aren't per-cpu, scale max_active according to #cpus */
#define WQ_UNBOUND_MAX_ACTIVE	WQ_MAX_ACTIVE

extern struct workqueue_struct *system_wq;
extern struct workqueue_struct *system_highpri_wq;
extern struct workqueue_struct *system_long_wq;
extern struct workqueue_struct *system_unbound_wq;
extern struct workqueue_struct *system_freezable_wq;

extern struct workqueue_struct *
alloc_workqueue(const char *fmt, unsigned int flags,
		int max_active, ...) __printf(1, 4);

#define alloc_ordered_workqueue(fmt, flags, args...)			\
	alloc_workqueue(fmt, WQ_UNBOUND | __WQ_ORDERED | (flags), 1, ##args)

#define create_workqueue(name)						\
	alloc_workqueue("%s", __WQ_LEGACY | WQ_MEM_RECLAIM, 1, (name))
#define create_freezable_workqueue(name)				\
	alloc_workqueue("%s", __WQ_LEGACY | WQ_FREEZABLE | WQ_UNBOUND |	\
			WQ_MEM_RECLAIM, 1, (name))
#define create_singlethread_workqueue(name)				\
	alloc_ordered_workqueue("%s", __WQ_LEGACY | WQ_MEM_RECLAIM, name)

extern void destroy_workqueue(struct workqueue_struct *wq);

struct workqueue_attrs *alloc_workqueue_attrs(gfp_t gfp_mask);
void free_workqueue_attrs(struct workqueue_attrs *attrs);
int apply_workqueue_attrs(struct workqueue_struct *wq,
			  const struct workqueue_attrs *attrs);

extern bool queue_work(struct workqueue_struct *wq,
		       struct work_struct *work);
extern bool queue_delayed_work(struct workqueue_struct *wq,
			struct delayed_work *work, unsigned long delay);
extern bool mod_delayed_work(struct workqueue_struct *wq,
			struct delayed_work *dwork, unsigned long delay);

extern void flush_workqueue(struct workqueue_struct *wq);
extern void drain_workqueue(struct workqueue_struct *wq);

extern int schedule_on_each_cpu(work_func_t func);

extern bool flush_work(struct work_struct *work);
extern bool cancel_work_sync(struct work_struct *work);

extern bool flush_delayed_work(struct delayed_work *dwork);
extern bool cancel_delayed_work(struct delayed_work *dwork);
extern bool cancel_delayed_work_sync(struct delayed_work *dwork);

extern void workqueue_set_max_active(struct workqueue_struct *wq,
				     int max_active);
extern bool current_is_workqueue_rescuer(void);
extern bool workqueue_congested(int cpu, struct workqueue_struct *wq);
extern unsigned int work_busy(struct work_struct *work);
extern __printf(1, 2) void set_worker_desc(const char *fmt, ...);
extern void print_worker_info(const char *log_lvl, struct task_struct *task);
extern void show_workqueue_state(void);

static inline bool schedule_work_on(int cpu, struct work_struct *work)
{
	return queue_work(system_wq, work);
}

static inline bool schedule_work(struct work_struct *work)
{
	return queue_work(system_wq, work);
}

static inline void flush_scheduled_work(void)
{
	flush_workqueue(system_wq);
}

static inline bool schedule_delayed_work_on(int cpu, struct delayed_work *dwork,
					    unsigned long delay)
{
	return queue_delayed_work(system_wq, dwork, delay);
}

static inline bool schedule_delayed_work(struct delayed_work *dwork,
					 unsigned long delay)
{
	return queue_delayed_work(system_wq, dwork, delay);
}

#endif /* __TOOLS_LINUX_WORKQUEUE_H */
