/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _LINUX_SCHED_SIGNAL_H
#define _LINUX_SCHED_SIGNAL_H

static inline int fatal_signal_pending(struct task_struct *p)
{
	return 0;
}

static inline int signal_pending(struct task_struct *p)
{
	return 0;
}

static inline int signal_pending_state(unsigned int state, struct task_struct *p)
{
	return 0;
}

#endif /* _LINUX_SCHED_SIGNAL_H */

