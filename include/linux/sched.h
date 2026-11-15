#ifndef __TOOLS_LINUX_SCHED_H
#define __TOOLS_LINUX_SCHED_H

#include <pthread.h>
#include <time.h>
#include <linux/atomic.h>
#include <linux/bug.h>
#include <linux/jiffies.h>
#include <linux/time64.h>

#define TASK_RUNNING		0
#define TASK_INTERRUPTIBLE	1
#define TASK_UNINTERRUPTIBLE	2
#define TASK_WAKEKILL		128
#define TASK_NOLOAD		1024
#define TASK_FREEZABLE		(1U << 14)

/* Convenience macros for the sake of set_task_state */
#define TASK_KILLABLE		(TASK_WAKEKILL | TASK_UNINTERRUPTIBLE)

#define TASK_IDLE		(TASK_UNINTERRUPTIBLE | TASK_NOLOAD)

#define TASK_COMM_LEN 16

#define PF_MEMALLOC_NOIO 0x00080000	/* Allocating memory without IO involved */
#define PF_KTHREAD	0x00200000	/* I am a kernel thread */
#define PF_MEMALLOC_NORECLAIM	0x00800000	/* All allocation requests will clear __GFP_DIRECT_RECLAIM */
#define PF_MEMALLOC_NOWARN	0x01000000	/* All allocation requests will inherit __GFP_NOWARN */

struct task_struct {
	int			(*thread_fn)(void *);
	void			*thread_data;

	int			prio;

	atomic_t		usage;
	int			state;

	/* kthread: */
	unsigned long		kthread_flags;

	unsigned		flags;

	bool			on_cpu;
	char			comm[TASK_COMM_LEN];
	pid_t			pid;

	struct bio_list		*bio_list;

	struct {
		u64		sum_exec_runtime;
		u64		exec_start;
	}			se;
};

/* os/linux_user/sched.rs */
struct task_struct *get_current(void);
#define current get_current()

void linux_thread_init(void);
void linux_thread_exit(void);

#define __set_current_state(state_value)		\
	do { current->state = (state_value); } while (0)
#define set_current_state(state_value)			\
	smp_store_mb(current->state, (state_value))

static inline struct task_struct *get_task_struct(struct task_struct *task)
{
	atomic_inc(&task->usage);
	return task;

}

extern void __put_task_struct(struct task_struct *t);

static inline void put_task_struct(struct task_struct *t)
{
	if (atomic_dec_and_test(&t->usage))
		__put_task_struct(t);
}

void schedule(void);

long schedule_timeout(long timeout);

static inline long io_schedule_timeout(long timeout)
{
	return schedule_timeout(timeout);
}

int wake_up_process(struct task_struct *);

static inline u64 ktime_get_real_seconds(void)
{
	struct timespec ts;

	clock_gettime(CLOCK_REALTIME, &ts);

	return ts.tv_sec;
}

static inline void ktime_get_coarse_real_ts64(struct timespec64 *ts)
{
	clock_gettime(CLOCK_REALTIME_COARSE, ts);
}

#endif /* __TOOLS_LINUX_SCHED_H */
