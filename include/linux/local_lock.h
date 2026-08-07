/* SPDX-License-Identifier: GPL-2.0 */
#ifndef __LINUX_LOCAL_LOCK_H
#define __LINUX_LOCAL_LOCK_H

/*
 * Userspace shim: in the kernel, local_lock pins the cpu and provides
 * per-cpu mutual exclusion (a real lock on PREEMPT_RT, irq/preempt
 * disable otherwise). The userspace tools don't have percpu data in the
 * kernel sense - these are the same no-ops the irqsave shim provides.
 */

typedef struct local_lock {
	unsigned char	pad;	/* zero-sized structs confuse bindgen */
} local_lock_t;

#define INIT_LOCAL_LOCK(name)	{}

#define local_lock_init(l)		do {} while (0)

#define local_lock_irqsave(l, flags)		\
	do { (void) (l); flags = 0; } while (0)

#define local_unlock_irqrestore(l, flags)	\
	do { (void) (l); (void) (flags); } while (0)

#endif /* __LINUX_LOCAL_LOCK_H */
