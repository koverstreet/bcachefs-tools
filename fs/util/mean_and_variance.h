/* SPDX-License-Identifier: GPL-2.0 */
#ifndef MEAN_AND_VARIANCE_H_
#define MEAN_AND_VARIANCE_H_

#include <linux/log2.h>
#include <linux/math64.h>
#include <linux/types.h>

#include "util/mean_and_variance_gen.h"

/**
 * sgm_median_mad_step() - one stochastic-gradient update of median + MAD.
 * @median:	estimator state
 * @mad:	estimator state
 * @x:		new sample
 * @weight:	log2 of step rate; step = max(mad >> weight, 1)
 */
static inline void
sgm_median_mad_step(s64 *median, u64 *mad, s64 x, u8 weight)
{
	u64 step = max_t(u64, *mad >> weight, 1);

	if (x > *median)
		*median += step;
	else if (x < *median)
		*median -= step;

	u64 dev = abs(x - *median);

	if (dev > *mad)
		*mad += step;
	else if (dev < *mad && *mad > step)
		*mad -= step;
	else if (dev < *mad)
		*mad = 1;
}

/**
 * mean_and_variance_update() - update a mean_and_variance struct with @x.
 * @weight: 0 selects the all-time (Robbins-Monro 1/n) schedule;
 *	    nonzero is the EW step weight (half-life ≈ 2^weight samples).
 *
 * Always tracks exact sum + n. Median + MAD are updated via the same
 * stochastic-gradient step kernel; the only difference between the
 * all-time and EW variants is the step-rate schedule.
 */
static inline void
mean_and_variance_update(struct mean_and_variance *s, s64 x, u8 weight)
{
	s->n++;
	s->sum += x;

	if (s->n == 1) {
		s->median = x;
		s->mad = max_t(u64, abs(x), 1);
		return;
	}

	if (!weight)
		weight = ilog2(s->n);

	sgm_median_mad_step(&s->median, &s->mad, x, weight);
}

s64 mean_and_variance_get_mean(struct mean_and_variance s);
s64 mean_and_variance_get_median(struct mean_and_variance s);
u64 mean_and_variance_get_mad(struct mean_and_variance s);
u32 mean_and_variance_get_stddev(struct mean_and_variance s);

#endif // MEAN_AND_VAIRANCE_H_
