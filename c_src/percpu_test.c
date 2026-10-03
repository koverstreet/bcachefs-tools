// SPDX-License-Identifier: GPL-2.0

/*
 * Unit test for the userspace percpu shim's slot lifetime (linux/percpu.c).
 *
 * - Churn: four times as many short-lived threads as there are slots, one
 *   after another. Each must get a slot - exited threads give theirs back, or
 *   thread BCH_PERCPU_MAX_CPUS aborts the process - and what
 *   each added to a percpu counter must still be counted after it has gone,
 *   as a CPU's counters outlive the tasks that ran on it.
 * - Live threads never share a slot: concurrently live threads each have a
 *   distinct chunk, and a percpu counter they all hammer sums exactly.
 *
 * The body is C so it can use the percpu macros directly; src/percpu_test.rs
 * invokes rust_percpu_test() under `cargo test`.
 */

#include <pthread.h>
#include <stdio.h>

#include "libbcachefs.h"

#include <linux/percpu.h>

#include "rust_shims.h"

#define TEST_FAIL(...) ({ fprintf(stderr, "rust_percpu_test FAIL: " __VA_ARGS__); fputc('\n', stderr); 1; })

#define CHURN_THREADS	(4 * BCH_PERCPU_MAX_CPUS)
#define LIVE_THREADS	16
#define LIVE_ADDS	10000

static u64 percpu_sum(u64 __percpu *ctr)
{
	u64 sum = 0;

	for (int cpu = 0; cpu < bch_percpu_nr_cpus; cpu++)
		sum += *per_cpu_ptr(ctr, cpu);
	return sum;
}

static void *churn_thread(void *arg)
{
	u64 __percpu *ctr = arg;

	this_cpu_add(*ctr, 1);
	return NULL;
}

static int test_churn(void)
{
	u64 __percpu *ctr = alloc_percpu(u64);
	int fails = 0;

	if (!ctr)
		return TEST_FAIL("churn: alloc_percpu failed");

	/*
	 * Without slots coming back, thread number BCH_PERCPU_MAX_CPUS aborts
	 * the process. Counting slots used instead wouldn't work: cargo runs
	 * other tests' threads alongside this.
	 */
	for (unsigned i = 0; i < CHURN_THREADS; i++) {
		pthread_t t;

		if (pthread_create(&t, NULL, churn_thread, ctr)) {
			fails += TEST_FAIL("churn: pthread_create %u failed", i);
			break;
		}
		pthread_join(t, NULL);
	}

	u64 sum = percpu_sum(ctr);
	if (sum != CHURN_THREADS)
		fails += TEST_FAIL("churn: counter sums to %llu, want %u",
				   (unsigned long long) sum, CHURN_THREADS);

	free_percpu(ctr);
	return fails;
}

struct live_args {
	u64 __percpu		*ctr;
	pthread_barrier_t	*barrier;
	void			*chunk;
};

static void *live_thread(void *arg)
{
	struct live_args *a = arg;

	this_cpu_add(*a->ctr, 0);
	a->chunk = bch_percpu_my_chunk;

	/* Everyone holds a slot at once before anyone can exit and free one: */
	pthread_barrier_wait(a->barrier);

	for (unsigned i = 0; i < LIVE_ADDS; i++)
		this_cpu_add(*a->ctr, 1);

	pthread_barrier_wait(a->barrier);
	return NULL;
}

static int test_live_distinct(void)
{
	u64 __percpu *ctr = alloc_percpu(u64);
	struct live_args args[LIVE_THREADS];
	pthread_t threads[LIVE_THREADS];
	pthread_barrier_t barrier;
	unsigned nr_started = 0;
	int fails = 0;

	if (!ctr)
		return TEST_FAIL("live: alloc_percpu failed");

	pthread_barrier_init(&barrier, NULL, LIVE_THREADS);

	for (unsigned i = 0; i < LIVE_THREADS; i++) {
		args[i] = (struct live_args) { .ctr = ctr, .barrier = &barrier };
		if (pthread_create(&threads[i], NULL, live_thread, &args[i])) {
			/* the barrier would never fill - can't continue */
			fprintf(stderr, "rust_percpu_test FAIL: live: pthread_create %u failed\n", i);
			abort();
		}
		nr_started++;
	}

	for (unsigned i = 0; i < nr_started; i++)
		pthread_join(threads[i], NULL);

	for (unsigned i = 0; i < LIVE_THREADS; i++)
		for (unsigned j = i + 1; j < LIVE_THREADS; j++)
			if (args[i].chunk == args[j].chunk)
				fails += TEST_FAIL("live: threads %u and %u share chunk %p",
						   i, j, args[i].chunk);

	u64 sum = percpu_sum(ctr);
	if (sum != (u64) LIVE_THREADS * LIVE_ADDS)
		fails += TEST_FAIL("live: counter sums to %llu, want %u",
				   (unsigned long long) sum, LIVE_THREADS * LIVE_ADDS);

	pthread_barrier_destroy(&barrier);
	free_percpu(ctr);
	return fails;
}

int rust_percpu_test(void)
{
	int fails = test_churn() + test_live_distinct();

	if (!fails)
		fprintf(stderr, "rust_percpu_test: all cases passed\n");
	return fails;
}
