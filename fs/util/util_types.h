/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_UTIL_UTIL_TYPES_H
#define _BCACHEFS_UTIL_UTIL_TYPES_H

#ifdef __KERNEL__

#include <linux/capability.h>

#endif

struct closure;

#include "printbuf.h"

#ifdef __KERNEL__

#else

#include <uuid/uuid.h>

#endif

DEFINE_DARRAY_NAMED(bch_stacktrace, unsigned long);

struct bch_ratelimit {
	/* Next time we want to do some work, in nanoseconds */
	u64			next;

	/*
	 * Rate at which we want to do work, in units per nanosecond
	 * The units here correspond to the units passed to
	 * bch2_ratelimit_increment()
	 */
	unsigned		rate;
};

struct bch_pd_controller {
	struct bch_ratelimit	rate;
	unsigned long		last_update;

	s64			last_actual;
	s64			smoothed_derivative;

	unsigned		p_term_inverse;
	unsigned		d_smooth;
	unsigned		d_term;

	/* for exporting to sysfs (no effect on behavior) */
	s64			last_derivative;
	s64			last_proportional;
	s64			last_change;
	s64			last_target;

	/*
	 * If true, the rate will not increase if bch2_ratelimit_delay()
	 * is not being called often enough.
	 */
	bool			backpressure;
};

#ifdef __KERNEL__

#include <linux/cpumask.h>

#include <linux/numa.h>

#endif

#include <linux/uuid.h>

#include <linux/sched/mm.h>

struct memalloc_flags { unsigned flags; };

#endif /* _BCACHEFS_UTIL_UTIL_TYPES_H */
