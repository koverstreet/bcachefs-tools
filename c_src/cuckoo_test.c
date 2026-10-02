// SPDX-License-Identifier: GPL-2.0

/*
 * Unit test for the cuckoo u64 set (fs/util/cuckoo.h).
 *
 * - Random adds, deletes and lookups over a small key range, checked against
 *   a reference bitmap after every operation and swept in full periodically.
 *   Starting from a two-slot table, the set grows through many rehashes and
 *   evictions, and a small key range means adds of members already present
 *   and deletes of keys never added are common.
 * - A bulk run: many random keys from the full u64 range, all present
 *   afterwards, and random probes absent.
 * - clear() empties the set and leaves it usable.
 *
 * The test body is plain C so it can use the static inlines directly; a Rust
 * #[test] wrapper (src/cuckoo_test.rs) invokes rust_cuckoo_test() so it runs
 * under `cargo test` with no kernel.
 */

#include <stdio.h>

#include "libbcachefs.h"

#include "fs/util/cuckoo.h"

#include "rust_shims.h"

#define TEST_FAIL(...) ({ fprintf(stderr, "rust_cuckoo_test FAIL: " __VA_ARGS__); fputc('\n', stderr); 1; })

#define KEY_RANGE	4096
#define RANDOM_OPS	200000
#define BULK_KEYS	100000

/* deterministic xorshift so runs are reproducible */
static u64 cuckoo_rng(u64 *state)
{
	u64 x = *state;
	x ^= x << 13;
	x ^= x >> 7;
	x ^= x << 17;
	return *state = x;
}

static int check_all(const struct cuckoo_u64 *t, const bool *ref, size_t ref_nr, unsigned op)
{
	int fails = 0;

	if (t->nr != ref_nr)
		fails += TEST_FAIL("op %u: nr %zu, want %zu", op, t->nr, ref_nr);

	for (u64 k = 1; k < KEY_RANGE; k++)
		if (cuckoo_u64_test(t, k) != ref[k])
			fails += TEST_FAIL("op %u: test(%llu) %u, want %u",
					   op, (unsigned long long) k, cuckoo_u64_test(t, k), ref[k]);
	return fails;
}

static int test_random_ops(void)
{
	struct cuckoo_u64 t;
	bool ref[KEY_RANGE] = {};
	size_t ref_nr = 0;
	u64 seed = 0x9e3779b97f4a7c15ULL;
	int fails = 0;

	if (cuckoo_u64_init(&t, 1, GFP_KERNEL))
		return TEST_FAIL("init failed");

	for (unsigned op = 0; op < RANDOM_OPS && !fails; op++) {
		u64 r = cuckoo_rng(&seed);
		u64 k = 1 + r % (KEY_RANGE - 1);

		/* bias towards adds, so the set grows before it churns: */
		if ((r >> 32) % 3) {
			if (cuckoo_u64_add(&t, k, GFP_KERNEL))
				fails += TEST_FAIL("op %u: add(%llu) failed", op, (unsigned long long) k);
			ref_nr += !ref[k];
			ref[k] = true;
		} else {
			bool had = cuckoo_u64_del(&t, k);

			if (had != ref[k])
				fails += TEST_FAIL("op %u: del(%llu) returned %u, want %u",
						   op, (unsigned long long) k, had, ref[k]);
			ref_nr -= ref[k];
			ref[k] = false;
		}

		if (cuckoo_u64_test(&t, k) != ref[k])
			fails += TEST_FAIL("op %u: test(%llu) after op wrong", op, (unsigned long long) k);

		if (!(op % 10000))
			fails += check_all(&t, ref, ref_nr, op);
	}

	fails += check_all(&t, ref, ref_nr, RANDOM_OPS);

	cuckoo_u64_clear(&t);
	memset(ref, 0, sizeof(ref));
	fails += check_all(&t, ref, 0, RANDOM_OPS + 1);

	if (cuckoo_u64_add(&t, 7, GFP_KERNEL) || !cuckoo_u64_test(&t, 7) || t.nr != 1)
		fails += TEST_FAIL("add after clear failed");

	cuckoo_u64_exit(&t);
	return fails;
}

static int test_bulk(void)
{
	struct cuckoo_u64 t;
	u64 *keys = calloc(BULK_KEYS, sizeof(u64));
	u64 seed = 0x123456789abcdefULL;
	int fails = 0;

	if (!keys || cuckoo_u64_init(&t, 4, GFP_KERNEL)) {
		free(keys);
		return TEST_FAIL("bulk: allocation failed");
	}

	for (unsigned i = 0; i < BULK_KEYS; i++) {
		keys[i] = cuckoo_rng(&seed) | 1;	/* never 0 */
		if (cuckoo_u64_add(&t, keys[i], GFP_KERNEL))
			fails += TEST_FAIL("bulk: add %u failed", i);
	}

	/* xorshift doesn't repeat within a run this short: all distinct */
	if (t.nr != BULK_KEYS)
		fails += TEST_FAIL("bulk: nr %zu, want %u", t.nr, BULK_KEYS);

	for (unsigned i = 0; i < BULK_KEYS; i++)
		if (!cuckoo_u64_test(&t, keys[i]))
			fails += TEST_FAIL("bulk: key %u missing", i);

	/* even keys were never added: */
	for (unsigned i = 0; i < 10000; i++)
		if (cuckoo_u64_test(&t, cuckoo_rng(&seed) & ~1ULL))
			fails += TEST_FAIL("bulk: probe %u found a key never added", i);

	if ((1UL << t.bits) > 8 * roundup_pow_of_two(BULK_KEYS))
		fails += TEST_FAIL("bulk: table grew to %lu slots for %u keys",
				   1UL << t.bits, BULK_KEYS);

	cuckoo_u64_exit(&t);
	free(keys);
	return fails;
}

int rust_cuckoo_test(void)
{
	int fails = test_random_ops() + test_bulk();

	if (!fails)
		fprintf(stderr, "rust_cuckoo_test: all cases passed\n");
	return fails;
}
