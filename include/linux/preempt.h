#ifndef __LINUX_PREEMPT_H
#define __LINUX_PREEMPT_H

#include <linux/cleanup.h>
#include <linux/irqflags.h>

extern void preempt_disable(void);
extern void preempt_enable(void);

#define sched_preempt_enable_no_resched()	preempt_enable()
#define preempt_enable_no_resched()		preempt_enable()
#define preempt_check_resched()			do { } while (0)

#define preempt_disable_notrace()		preempt_disable()
#define preempt_enable_no_resched_notrace()	preempt_enable()
#define preempt_enable_notrace()		preempt_enable()
#define preemptible()				0

DEFINE_LOCK_GUARD_0(preempt, preempt_disable(), preempt_enable())
DEFINE_LOCK_GUARD_0(preempt_notrace, preempt_disable_notrace(), preempt_enable_notrace())

#endif /* __LINUX_PREEMPT_H */
