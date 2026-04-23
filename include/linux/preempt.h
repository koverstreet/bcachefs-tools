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

/* Kernel migrate_disable() prevents CPU-migration but NOT preemption
 * — weaker than preempt_disable(). In userspace there's no CPU-pin
 * guarantee to provide, and percpu access sites already bracket
 * themselves with preempt_disable() where they need serialization.
 *
 * Mapping these to preempt_disable() would serialize every trans
 * through the global preempt_lock for the whole trans lifetime and
 * deadlock when a trans waits on anything that itself wants a trans
 * (discard completion, journal, etc.). No-op is correct. */
#define migrate_disable()			do { } while (0)
#define migrate_enable()			do { } while (0)

DEFINE_LOCK_GUARD_0(preempt, preempt_disable(), preempt_enable())
DEFINE_LOCK_GUARD_0(preempt_notrace, preempt_disable_notrace(), preempt_enable_notrace())

#endif /* __LINUX_PREEMPT_H */
