// SPDX-License-Identifier: GPL-2.0

//! A mean_and_variance's estimators - see mean_and_variance_types.rs for the
//! design: an exact sum and count for the mean, and a streaming median and
//! MAD, each stepped by a fraction of the MAD per sample.
//!
//! The arithmetic is C's, as the kernel builds it (-fno-strict-overflow):
//! signed and unsigned both wrap. And the mean divides unsigned, as C's
//! div64_u64() did - a negative sum isn't a negative mean; time_stats', the
//! one user, sums durations.

#![allow(non_camel_case_types)]

use crate::c;
use cstruct_macros::MaybeZeroable;

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
#[repr(C)]
#[derive(Clone, Copy, Default, MaybeZeroable)]
pub struct mean_and_variance {
    pub n: i64,
    pub sum: i64,
    pub median: i64,
    pub mad: u64,
}

/// One stochastic-gradient step of @median and @mad toward @x: by the MAD
/// shifted down @weight, at least 1. The MAD bottoms out at 1, never 0, so
/// the step never stalls.
fn sgm_median_mad_step(median: &mut i64, mad: &mut u64, x: i64, weight: u8) {
    let step = (*mad >> weight).max(1);

    if x > *median {
        *median = median.wrapping_add(step as i64);
    } else if x < *median {
        *median = median.wrapping_sub(step as i64);
    }

    let dev = x.wrapping_sub(*median).unsigned_abs();
    if dev > *mad {
        *mad = mad.wrapping_add(step);
    } else if dev < *mad && *mad > step {
        *mad -= step;
    } else if dev < *mad {
        *mad = 1;
    }
}

impl c::mean_and_variance {
    /// @x added. @weight 0 is the all-time schedule, a step of MAD/n;
    /// otherwise it's the weighted one's shift, a half-life of 2^weight
    /// samples.
    #[inline]
    pub fn update(&mut self, x: i64, weight: u8) {
        self.n += 1;
        self.sum = self.sum.wrapping_add(x);

        if self.n == 1 {
            self.median = x;
            self.mad = x.unsigned_abs().max(1);
            return;
        }

        let weight = if weight != 0 { weight } else { (self.n as u64).ilog2() as u8 };
        sgm_median_mad_step(&mut self.median, &mut self.mad, x, weight);
    }

    pub fn mean(&self) -> i64 {
        if self.n != 0 { ((self.sum as u64) / (self.n as u64)) as i64 } else { 0 }
    }

    pub fn median(&self) -> i64 {
        self.median
    }

    pub fn mad(&self) -> u64 {
        if self.n != 0 { self.mad } else { 0 }
    }

    /// 1.4826 · MAD, about σ for Gaussian inputs: (mad * 1518) >> 10.
    pub fn stddev(&self) -> u32 {
        if self.n != 0 { (self.mad.wrapping_mul(1518) >> 10) as u32 } else { 0 }
    }
}

// ── For C ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(s: &mut c::mean_and_variance, x: i64, weight: u8, times: usize) {
        for _ in 0..times {
            s.update(x, weight);
        }
    }

    #[test]
    fn basic() {
        let mut s = c::mean_and_variance::default();

        feed(&mut s, 2, 0, 2);
        assert_eq!((s.mean(), s.n), (2, 2));

        feed(&mut s, 4, 0, 2);
        assert_eq!((s.mean(), s.n), (3, 4));

        // constant samples: MAD decays toward 1
        feed(&mut s, 3, 0, 4096);
        assert!(s.mad() <= 4, "mad {}", s.mad());
    }

    #[test]
    fn weighted_constant() {
        let mut s = c::mean_and_variance::default();
        feed(&mut s, 100, 8, 4096);
        assert_eq!(s.median(), 100);
        assert!(s.mad() <= 4, "mad {}", s.mad());
    }

    /// From one value to another: the median moves toward the new one,
    /// monotonically while only fed samples above it.
    #[test]
    fn weighted_step() {
        let mut s = c::mean_and_variance::default();
        s.update(100, 8);

        let mut prev = s.median();
        for _ in 0..4096 {
            s.update(1000, 8);
            assert!(s.median() >= prev, "median {} after {prev}", s.median());
            prev = s.median();
        }
        assert!(s.median() >= 900, "median {}", s.median());
    }

    /// One huge outlier grows the MAD by at most a step, ~mad/256.
    #[test]
    fn weighted_outlier() {
        let mut s = c::mean_and_variance::default();
        feed(&mut s, 100, 8, 1024);

        let before = s.mad();
        s.update(1_000_000, 8);
        assert!(s.mad() <= before + (before >> 7) + 1, "mad {} from {before}", s.mad());
    }
}
