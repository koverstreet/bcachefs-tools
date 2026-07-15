#ifndef _LINUX_WAIT_H
#define _LINUX_WAIT_H

#include <pthread.h>
#include <linux/bitmap.h>
#include <linux/list.h>
#include <linux/spinlock.h>

typedef struct __wait_queue wait_queue_t;
typedef int (*wait_queue_func_t)(wait_queue_t *wait, unsigned mode, int flags, void *key);

#define WQ_FLAG_EXCLUSIVE	0x01

struct __wait_queue {
	unsigned int		flags;
	void			*private;
	wait_queue_func_t	func;
	struct list_head	task_list;
};

struct wait_queue_head {
	spinlock_t		lock;
	struct list_head	task_list;
};

typedef struct wait_queue_head wait_queue_head_t;

void wake_up(wait_queue_head_t *);
void wake_up_all(wait_queue_head_t *);
void prepare_to_wait(wait_queue_head_t *q, wait_queue_t *wait, int state);
void finish_wait(wait_queue_head_t *q, wait_queue_t *wait);
int autoremove_wake_function(wait_queue_t *wait, unsigned mode, int sync, void *key);
int default_wake_function(wait_queue_t *wait, unsigned mode, int flags, void *key);

#define DECLARE_WAITQUEUE(name, tsk)					\
	wait_queue_t name = {						\
		.private	= tsk,					\
		.func		= default_wake_function,		\
		.task_list	= { NULL, NULL }			\
	}

#define __WAIT_QUEUE_HEAD_INITIALIZER(name) {				\
	.lock		= __SPIN_LOCK_UNLOCKED(name.lock),		\
	.task_list	= { &(name).task_list, &(name).task_list } }

#define DECLARE_WAIT_QUEUE_HEAD(name) \
	struct wait_queue_head name = __WAIT_QUEUE_HEAD_INITIALIZER(name)

static inline void init_waitqueue_head(wait_queue_head_t *q)
{
	spin_lock_init(&q->lock);
	INIT_LIST_HEAD(&q->task_list);
}

#define DEFINE_WAIT(name)						\
	wait_queue_t name = {						\
		.private	= current,				\
		.func		= autoremove_wake_function,		\
		.task_list	= LIST_HEAD_INIT((name).task_list),	\
	}

#define ___wait_cond_timeout(condition)					\
({									\
	bool __cond = (condition);					\
	if (__cond && !__ret)						\
		__ret = 1;						\
	__cond || !__ret;						\
})

#define ___wait_event(wq, condition, state, exclusive, ret, cmd)	\
({									\
	DEFINE_WAIT(__wait);						\
	long __ret = ret;						\
									\
	for (;;) {							\
		prepare_to_wait(&wq, &__wait, state);			\
		if (condition)						\
			break;						\
		cmd;							\
	}								\
	finish_wait(&wq, &__wait);					\
	__ret;								\
})

#define __wait_event(wq, condition)					\
	(void)___wait_event(wq, condition, TASK_UNINTERRUPTIBLE, 0, 0,	\
			    schedule())

#define wait_event(wq, condition)					\
do {									\
	if (condition)							\
		break;							\
	__wait_event(wq, condition);					\
} while (0)

#define wait_event_freezable(wq, condition)	({wait_event(wq, condition); 0; })
#define wait_event_killable(wq, condition)	({wait_event(wq, condition); 0; })
#define wait_event_interruptible(wq, condition)	({wait_event(wq, condition); 0; })
#define wait_event_state(wq, condition, state)	({wait_event(wq, condition); 0; })

#define __wait_event_timeout(wq, condition, timeout)			\
	___wait_event(wq, ___wait_cond_timeout(condition),		\
		      TASK_UNINTERRUPTIBLE, 0, timeout,			\
		      __ret = schedule_timeout(__ret))

#define wait_event_freezable_timeout(wq, condition, timeout)	wait_event_timeout(wq, condition, timeout)

#define wait_event_timeout(wq, condition, timeout)			\
({									\
	long __ret = timeout;						\
	if (!___wait_cond_timeout(condition))				\
		__ret = __wait_event_timeout(wq, condition, timeout);	\
	__ret;								\
})

void wake_up_bit(void *, int);
void __wait_on_bit(void *, int, unsigned);
void __wait_on_bit_lock(void *, int, unsigned);

static inline int
wait_on_bit(unsigned long *word, int bit, unsigned mode)
{
	if (!test_bit(bit, word))
		return 0;
	__wait_on_bit(word, bit, mode);
	return 0;
}

static inline int
wait_on_bit_lock(unsigned long *word, int bit, unsigned mode)
{
	if (!test_and_set_bit(bit, word))
		return 0;
	__wait_on_bit_lock(word, bit, mode);
	return 0;
}

#define wait_on_bit_io(w, b, m)			wait_on_bit(w, b, m)
#define wait_on_bit_lock_io(w, b, m)		wait_on_bit_lock(w, b, m)

#endif /* _LINUX_WAIT_H */
