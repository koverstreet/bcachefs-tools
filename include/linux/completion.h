#ifndef __LINUX_COMPLETION_H
#define __LINUX_COMPLETION_H

/*
 * (C) Copyright 2001 Linus Torvalds
 *
 * Atomic wait-for-completion handler data structures.
 * See kernel/sched/completion.c for details.
 */

#include <linux/wait.h>

struct completion {
	unsigned int done;
	wait_queue_head_t wait;
};

#define DECLARE_COMPLETION(work)					\
	struct completion work = {					\
		.done = 0,						\
		.wait = __WAIT_QUEUE_HEAD_INITIALIZER((work).wait)	\
	}

#define DECLARE_COMPLETION_ONSTACK(work) DECLARE_COMPLETION(work)

static inline void init_completion(struct completion *x)
{
	x->done = 0;
	init_waitqueue_head(&x->wait);
}

static inline void reinit_completion(struct completion *x)
{
	x->done = 0;
}

void complete(struct completion *);
void wait_for_completion(struct completion *);
unsigned long wait_for_completion_timeout(struct completion *, unsigned long);

#define wait_for_completion_interruptible(x) (wait_for_completion(x), 0)

#endif
