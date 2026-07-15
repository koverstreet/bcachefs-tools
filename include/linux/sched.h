#ifndef __TOOLS_LINUX_SCHED_H
#define __TOOLS_LINUX_SCHED_H

#include <pthread.h>
#include <time.h>
#include <linux/atomic.h>
#include <linux/bug.h>
#include <linux/completion.h>
#include <linux/jiffies.h>
#include <linux/rwsem.h>
#include <linux/time64.h>

#define TASK_RUNNING		0
#define TASK_INTERRUPTIBLE	1
#define TASK_UNINTERRUPTIBLE	2
#define __TASK_STOPPED		4
#define __TASK_TRACED		8
/* in tsk->exit_state */
#define EXIT_DEAD		16
#define EXIT_ZOMBIE		32
#define EXIT_TRACE		(EXIT_ZOMBIE | EXIT_DEAD)
/* in tsk->state again */
#define TASK_DEAD		64
#define TASK_WAKEKILL		128
#define TASK_WAKING		256
#define TASK_PARKED		512
#define TASK_NOLOAD		1024
#define TASK_NEW		2048
#define TASK_IDLE_WORKER	4096
#define TASK_STATE_MAX		8192
#define TASK_FREEZABLE		(1U << 14)

/* Convenience macros for the sake of set_task_state */
#define TASK_KILLABLE		(TASK_WAKEKILL | TASK_UNINTERRUPTIBLE)
#define TASK_STOPPED		(TASK_WAKEKILL | __TASK_STOPPED)
#define TASK_TRACED		(TASK_WAKEKILL | __TASK_TRACED)

#define TASK_IDLE		(TASK_UNINTERRUPTIBLE | TASK_NOLOAD)

/* Convenience macros for the sake of wake_up */
#define TASK_NORMAL		(TASK_INTERRUPTIBLE | TASK_UNINTERRUPTIBLE)
#define TASK_ALL		(TASK_NORMAL | __TASK_STOPPED | __TASK_TRACED)

#define TASK_COMM_LEN 16

#define PF_EXITING	0x00000004	/* getting shut down */
#define PF_EXITPIDONE	0x00000008	/* pi exit done on shut down */
#define PF_VCPU		0x00000010	/* I'm a virtual CPU */
#define PF_WQ_WORKER	0x00000020	/* I'm a workqueue worker */
#define PF_FORKNOEXEC	0x00000040	/* forked but didn't exec */
#define PF_MCE_PROCESS  0x00000080      /* process policy on mce errors */
#define PF_SUPERPRIV	0x00000100	/* used super-user privileges */
#define PF_DUMPCORE	0x00000200	/* dumped core */
#define PF_SIGNALED	0x00000400	/* killed by a signal */
#define PF_MEMALLOC	0x00000800	/* Allocating memory */
#define PF_NPROC_EXCEEDED 0x00001000	/* set_user noticed that RLIMIT_NPROC was exceeded */
#define PF_USED_MATH	0x00002000	/* if unset the fpu must be initialized before use */
#define PF_USED_ASYNC	0x00004000	/* used async_schedule*(), used by module init */
#define PF_NOFREEZE	0x00008000	/* this thread should not be frozen */
#define PF_FROZEN	0x00010000	/* frozen for system suspend */
#define PF_FSTRANS	0x00020000	/* inside a filesystem transaction */
#define PF_KSWAPD	0x00040000	/* I am kswapd */
#define PF_MEMALLOC_NOFS 0x00040000	/* All allocations inherit GFP_NOFS. See memalloc_nfs_save() */
#define PF_MEMALLOC_NOIO 0x00080000	/* Allocating memory without IO involved */
#define PF_LESS_THROTTLE 0x00100000	/* Throttle me less: I clean memory */
#define PF_KTHREAD	0x00200000	/* I am a kernel thread */
#define PF_RANDOMIZE	0x00400000	/* randomize virtual address space */
#define PF_MEMALLOC_NORECLAIM	0x00800000	/* All allocation requests will clear __GFP_DIRECT_RECLAIM */
#define PF_MEMALLOC_NOWARN	0x01000000	/* All allocation requests will inherit __GFP_NOWARN */
#define PF_SWAPWRITE	0x00800000	/* Allowed to write to swap */
#define PF_NO_SETAFFINITY 0x04000000	/* Userland is not allowed to meddle with cpus_allowed */
#define PF_MCE_EARLY    0x08000000      /* Early kill for mce process policy */
#define PF_MUTEX_TESTER	0x20000000	/* Thread belongs to the rt mutex tester */
#define PF_FREEZER_SKIP	0x40000000	/* Freezer should not count it as freezable */

struct task_struct {
	pthread_t		thread;

	int			(*thread_fn)(void *);
	void			*thread_data;

	int			prio;

	atomic_t		usage;
	int			state;

	/* kthread: */
	unsigned long		kthread_flags;
	struct completion	exited;

	unsigned		flags;

	bool			on_cpu;
	char			comm[TASK_COMM_LEN];
	pid_t			pid;

	struct bio_list		*bio_list;

	struct signal_struct	{
		struct rw_semaphore exec_update_lock;
	}			*signal, _signal;

	struct {
		u64		sum_exec_runtime;
		u64		exec_start;
	}			se;
};

#define MAX_RT_PRIO	0

extern __thread struct task_struct *current;

#define __set_task_state(tsk, state_value)		\
	do { (tsk)->state = (state_value); } while (0)
#define set_task_state(tsk, state_value)		\
	smp_store_mb((tsk)->state, (state_value))
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

static inline void cond_resched(void) {}
#define need_resched()	0

void schedule(void);

#define	MAX_SCHEDULE_TIMEOUT	LONG_MAX
long schedule_timeout(long timeout);

static inline void io_schedule(void)
{
	schedule();
}

static inline long io_schedule_timeout(long timeout)
{
	return schedule_timeout(timeout);
}

int wake_up_process(struct task_struct *);

static inline u64 ktime_get_seconds(void)
{
	struct timespec ts;

	clock_gettime(CLOCK_MONOTONIC, &ts);

	return ts.tv_sec;
}

static inline u64 ktime_get_real_ns(void)
{
	struct timespec ts;

	clock_gettime(CLOCK_REALTIME, &ts);
	return timespec_to_ns(&ts);
}

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

#define current_kernel_time64()	current_kernel_time()
#define CURRENT_TIME		(current_kernel_time())

#define sched_annotate_sleep()	do {} while (0)

unsigned int stack_trace_save_tsk(struct task_struct *task,
				  unsigned long *store, unsigned int size,
				  unsigned int skipnr);

#endif /* __TOOLS_LINUX_SCHED_H */
