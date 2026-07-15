#ifndef __TOOLS_LINUX_IRQFLAGS_H
#define __TOOLS_LINUX_IRQFLAGS_H

#include <linux/cleanup.h>

/*
 * Userspace has no hardware interrupts to disable; the percpu + irq
 * dance in kernel code becomes a no-op here.
 */

#define local_irq_save(flags)		((void)((flags) = 0))
#define local_irq_restore(flags)	((void)(flags))
#define local_irq_disable()		do { } while (0)
#define local_irq_enable()		do { } while (0)

DEFINE_LOCK_GUARD_0(irqsave, , )

#endif /* __TOOLS_LINUX_IRQFLAGS_H */
