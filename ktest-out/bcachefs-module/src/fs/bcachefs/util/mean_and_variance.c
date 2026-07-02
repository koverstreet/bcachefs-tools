// SPDX-License-Identifier: GPL-2.0
/*
 * Streaming median + MAD estimators (all-time and exponentially-weighted).
 *
 * Replaces an earlier sum-of-squares variance implementation; see the
 * struct comments in mean_and_variance.h for the streaming-estimator
 * design.
 */

#include <linux/bug.h>
#include <linux/compiler.h>
#include <linux/export.h>
#include <linux/limits.h>
#include <linux/math64.h>
#include <linux/module.h>

#include "mean_and_variance.h"

/**
 * mean_and_variance_get_mean() - get mean from @s
 * @s: mean and variance number of samples and their sums
 */
s64 mean_and_variance_get_mean(struct mean_and_variance s)
{
	return s.n ? div64_u64(s.sum, s.n) : 0;
}
EXPORT_SYMBOL_GPL(mean_and_variance_get_mean);

/**
 * mean_and_variance_get_mad() - get the all-time MAD (median absolute deviation)
 *
 * Tracked by stochastic-gradient median on |x − median| with a 1/n step
 * schedule. Robbins-Monro consistent: converges to the true MAD as n → ∞.
 */
u64 mean_and_variance_get_mad(struct mean_and_variance s)
{
	return s.n ? s.mad : 0;
}
EXPORT_SYMBOL_GPL(mean_and_variance_get_mad);

/**
 * mean_and_variance_get_stddev() - Gaussian-equivalent dispersion estimate
 *
 * Returns 1.4826 · MAD (~σ for Gaussian inputs), approximated as
 * (mad * 1518) >> 10 (~0.01% error). Preserves the existing "stddev"
 * readout label.
 */
u32 mean_and_variance_get_stddev(struct mean_and_variance s)
{
	return s.n ? (s.mad * 1518) >> 10 : 0;
}
EXPORT_SYMBOL_GPL(mean_and_variance_get_stddev);

/**
 * mean_and_variance_get_median() - get the current median estimate.
 */
s64 mean_and_variance_get_median(struct mean_and_variance s)
{
	return s.median;
}
EXPORT_SYMBOL_GPL(mean_and_variance_get_median);

MODULE_AUTHOR("Daniel B. Hill");
MODULE_LICENSE("GPL");
