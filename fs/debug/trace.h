/* SPDX-License-Identifier: GPL-2.0 */
#undef TRACE_SYSTEM
#define TRACE_SYSTEM bcachefs

#if !defined(_TRACE_BCACHEFS_H) || defined(TRACE_HEADER_MULTI_READ)

#include <linux/tracepoint.h>

DECLARE_EVENT_CLASS(fs_str,
	TP_PROTO(const char *fs, const char *str),
	TP_ARGS(fs, str),

	TP_STRUCT__entry(
		__array(char,		fs, 40			)
		__string(str,		str			)
	),

	TP_fast_assign(
		strscpy(__entry->fs, fs, sizeof(__entry->fs));
		__assign_str(str);
	),

	TP_printk("%s: %s", __entry->fs, __get_str(str))
);

/* BCH_NOCOUNTER_TRACEPOINTS(), BCH_PATH_TRACEPOINTS(): debug/trace_types.rs */
#include "debug/trace_gen.h"

#if !defined(CONFIG_BCACHEFS_PATH_TRACEPOINTS) && !defined(_TRACE_BCACHEFS_H)
#define x(n, ...)							\
	static inline void trace_##n(const char *fs, const char *s) {}\
	static inline bool trace_##n##_enabled(void) { return false; }
	BCH_PATH_TRACEPOINTS()
#undef x
#endif

#define x(n, ...)							\
	DEFINE_EVENT(fs_str, n,						\
		TP_PROTO(const char *fs, const char *str),		\
		TP_ARGS(fs, str)					\
	);
	BCH_PERSISTENT_COUNTERS()
	BCH_NOCOUNTER_TRACEPOINTS()
#ifdef CONFIG_BCACHEFS_PATH_TRACEPOINTS
	BCH_PATH_TRACEPOINTS()
#endif
#undef x

#define _TRACE_BCACHEFS_H
#endif /* _TRACE_BCACHEFS_H */

/* This part must be outside protection */
#undef TRACE_INCLUDE_PATH
#define TRACE_INCLUDE_PATH ../../fs/bcachefs/debug

#undef TRACE_INCLUDE_FILE
#define TRACE_INCLUDE_FILE trace

#include <trace/define_trace.h>
