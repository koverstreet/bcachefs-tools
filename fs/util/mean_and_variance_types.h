/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_UTIL_MEAN_AND_VARIANCE_TYPES_H
#define _BCACHEFS_UTIL_MEAN_AND_VARIANCE_TYPES_H

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

#endif /* _BCACHEFS_UTIL_MEAN_AND_VARIANCE_TYPES_H */
