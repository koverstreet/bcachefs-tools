// SPDX-License-Identifier: GPL-2.0
#include <kunit/test.h>

#include "mean_and_variance.h"

static void mean_and_variance_basic_test(struct kunit *test)
{
	struct mean_and_variance s = {};

	/*
	 * Mean is exact via sum/n. MAD is a streaming estimator (Robbins-Monro
	 * 1/n schedule) so we only assert convergence-shaped properties, not
	 * exact values.
	 */

	mean_and_variance_update(&s, 2, 0);
	mean_and_variance_update(&s, 2, 0);

	KUNIT_EXPECT_EQ(test, mean_and_variance_get_mean(s), 2);
	KUNIT_EXPECT_EQ(test, s.n, 2);

	mean_and_variance_update(&s, 4, 0);
	mean_and_variance_update(&s, 4, 0);

	KUNIT_EXPECT_EQ(test, mean_and_variance_get_mean(s), 3);
	KUNIT_EXPECT_EQ(test, s.n, 4);

	/* Many constant samples afterwards: MAD should decay toward 1. */
	for (int i = 0; i < 4096; i++)
		mean_and_variance_update(&s, 3, 0);
	KUNIT_EXPECT_LE(test, mean_and_variance_get_mad(s), 4);
}

/* Streaming median + MAD: sanity checks on convergence behaviour. */

static void mean_and_variance_weighted_constant_test(struct kunit *test)
{
	/* Many samples of a constant: median -> X, MAD -> small. */
	struct mean_and_variance s = { };

	for (int i = 0; i < 4096; i++) {
		mean_and_variance_update(&s, 100, 8);
	}

	KUNIT_EXPECT_EQ(test, mean_and_variance_get_median(s), 100);
	KUNIT_EXPECT_LE(test, mean_and_variance_get_mad(s), 4);
}

static void mean_and_variance_weighted_step_test(struct kunit *test)
{
	/*
	 * Switch from one value to another and verify the median moves toward
	 * the new value (monotonically, while only feeding samples above it).
	 */
	struct mean_and_variance s = { };
	s64 prev_median = 0;

	mean_and_variance_update(&s, 100, 8);

	prev_median = mean_and_variance_get_median(s);
	for (int i = 0; i < 4096; i++) {
		mean_and_variance_update(&s, 1000, 8);
		s64 m = mean_and_variance_get_median(s);
		KUNIT_EXPECT_GE(test, m, prev_median);
		prev_median = m;
	}

	KUNIT_EXPECT_GE(test, mean_and_variance_get_median(s), 900);
}

static void mean_and_variance_weighted_outlier_test(struct kunit *test)
{
	/* One huge outlier shouldn't blow up the dispersion estimate. */
	struct mean_and_variance s = { };

	for (int i = 0; i < 1024; i++) {
		mean_and_variance_update(&s, 100, 8);
	}

	u64 mad_before = mean_and_variance_get_mad(s);
	mean_and_variance_update(&s, 1000000, 8);
	u64 mad_after = mean_and_variance_get_mad(s);

	/* MAD can only grow by ~mad_before/256 per sample; bounded growth. */
	KUNIT_EXPECT_LE(test, mad_after, mad_before + (mad_before >> 7) + 1);
}

static struct kunit_case mean_and_variance_test_cases[] = {
	KUNIT_CASE(mean_and_variance_basic_test),
	KUNIT_CASE(mean_and_variance_weighted_constant_test),
	KUNIT_CASE(mean_and_variance_weighted_step_test),
	KUNIT_CASE(mean_and_variance_weighted_outlier_test),
	{}
};

static struct kunit_suite mean_and_variance_test_suite = {
	.name		= "mean and variance tests",
	.test_cases	= mean_and_variance_test_cases
};

kunit_test_suite(mean_and_variance_test_suite);

MODULE_AUTHOR("Daniel B. Hill");
MODULE_DESCRIPTION("bcachefs filesystem mean and variance unit tests");
MODULE_LICENSE("GPL");
