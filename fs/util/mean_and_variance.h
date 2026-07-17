/* SPDX-License-Identifier: GPL-2.0 */
#ifndef MEAN_AND_VARIANCE_H_
#define MEAN_AND_VARIANCE_H_

#include <linux/log2.h>
#include <linux/math64.h>
#include <linux/types.h>

/*
 * Streaming median + MAD (median absolute deviation), via a
 * stochastic-gradient estimator on the L1 losses:
 *
 *	median minimizes E[|X − m|]	→ ∇ = −sign(X − m)
 *	MAD	minimizes E[||X − m| − d|]	→ ∇ = −sign(|X − m| − d)
 *
 * Two update modes share the same struct and the same step kernel:
 *
 *  - All-time:  mean_and_variance_update(s, x)
 *	 step ≈ mad / n (Robbins-Monro 1/n schedule via ilog2(n) shift).
 *	 Σα diverges, Σα² converges → the estimator is *consistent*,
 *	 converging to the true (lifetime) median and MAD as n → ∞.
 *	 Additionally maintains exact sum and count, so the true mean
 *	 is available via mean_and_variance_get_mean().
 *
 *  - Exponentially-weighted: mean_and_variance_update_weighted(s, x, w)
 *	 step = mad >> w (fixed weight). Asymptotic half-life is 2^w
 *	 samples; steady-state wobble is ~mad/2^w. Tracks the median of
 *	 the EW-weighted stream — sum and count fields are ignored.
 *
 * Per-sample work is shifts, adds, and sign bits: no multiplication,
 * no division, no sqrt, no u128. MAD is robust to outliers — a single
 * 100ms hiccup in a µs stream doesn't blow up the dispersion estimate.
 * The mad-scaled step makes both modes scale-invariant: same convergence
 * shape for ns- or s-magnitude inputs, no per-call-site tuning.
 *
 * For Gaussian inputs, σ ≈ 1.4826·MAD; the get_stddev helper returns
 * that scaled value so existing "stddev" readout labels keep meaning
 * roughly "spread around typical."
 */
struct mean_and_variance {
	s64	n;
	s64	sum;
	s64	median;
	u64	mad;
};

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
