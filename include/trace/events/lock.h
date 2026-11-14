/* SPDX-License-Identifier: GPL-2.0 */
#undef TRACE_SYSTEM
#define TRACE_SYSTEM lock

#if !defined(_TRACE_LOCK_H) || defined(TRACE_HEADER_MULTI_READ)
#define _TRACE_LOCK_H

#include <linux/sched.h>
#include <linux/tracepoint.h>

/* flags for lock:contention_begin */
#define LCB_F_SPIN	(1U << 0)
#define LCB_F_READ	(1U << 1)
#define LCB_F_WRITE	(1U << 2)
#define LCB_F_RT	(1U << 3)
#define LCB_F_PERCPU	(1U << 4)
#define LCB_F_MUTEX	(1U << 5)

TRACE_EVENT(contention_begin,

	TP_PROTO(void *lock, unsigned int flags),

	TP_ARGS(lock, flags),

	TP_STRUCT__entry(
		__field(void *, lock_addr)
		__field(unsigned int, flags)
	),

	TP_fast_assign(
		__entry->lock_addr = lock;
		__entry->flags = flags;
	),

	TP_printk("%p (flags=%s)", __entry->lock_addr,
		  __print_flags(__entry->flags, "|",
				{ LCB_F_SPIN,		"SPIN" },
				{ LCB_F_READ,		"READ" },
				{ LCB_F_WRITE,		"WRITE" },
				{ LCB_F_RT,		"RT" },
				{ LCB_F_PERCPU,		"PERCPU" },
				{ LCB_F_MUTEX,		"MUTEX" }
			  ))
);

TRACE_EVENT(contention_end,

	TP_PROTO(void *lock, int ret),

	TP_ARGS(lock, ret),

	TP_STRUCT__entry(
		__field(void *, lock_addr)
		__field(int, ret)
	),

	TP_fast_assign(
		__entry->lock_addr = lock;
		__entry->ret = ret;
	),

	TP_printk("%p (ret=%d)", __entry->lock_addr, __entry->ret)
);

#endif /* _TRACE_LOCK_H */

/* This part must be outside protection */
#include <trace/define_trace.h>
