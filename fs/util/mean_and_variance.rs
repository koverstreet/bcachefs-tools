// SPDX-License-Identifier: GPL-2.0

//! A mean_and_variance's estimators - see mean_and_variance_types.rs for the
//! design: an exact sum and count for the mean, and a streaming median and
//! MAD, each stepped by a fraction of the MAD per sample.
//!
//! The arithmetic is C's, as the kernel builds it (-fno-strict-overflow):
//! signed and unsigned both wrap. And the mean divides unsigned, as C's
//! div64_u64() did - a negative sum isn't a negative mean; time_stats', the
//! one user, sums durations.

use crate::c;
use cstruct_macros::c_export;

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

#[c_export]
pub fn mean_and_variance_update(s: &mut c::mean_and_variance, x: i64, weight: u8) {
    s.update(x, weight)
}

#[c_export]
pub fn mean_and_variance_get_mean(s: c::mean_and_variance) -> i64 {
    s.mean()
}

#[c_export]
pub fn mean_and_variance_get_median(s: c::mean_and_variance) -> i64 {
    s.median()
}

#[c_export]
pub fn mean_and_variance_get_mad(s: c::mean_and_variance) -> u64 {
    s.mad()
}

#[c_export]
pub fn mean_and_variance_get_stddev(s: c::mean_and_variance) -> u32 {
    s.stddev()
}

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
