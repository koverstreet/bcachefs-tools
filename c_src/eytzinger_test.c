// SPDX-License-Identifier: GPL-2.0

/*
 * Unit test for the eytzinger sort + search primitive and the darray 1-based
 * wrapper (darray_eytzinger1_sort / darray_eytzinger1_find) that
 * snapshot_id_dying() relies on.
 *
 * The eytzinger layout is subtle: eytzinger1_sort() rearranges elements into
 * a layout whose in-order traversal is ascending, and eytzinger1_find() does
 * a BST descent over that exact layout. The two must agree on element count
 * and base pointer, or find() silently returns the wrong entry - or NULL -
 * for a present key. The darray wrapper adds a sentinel at index 0 and passes
 * nr - 1 as the count; this pins down that off-by-one bookkeeping too.
 *
 * Each size runs TRIALS randomized rounds with distinct keys drawn from the
 * full u32 range (snapshot ids live near U32_MAX). Every round:
 *   - verifies the sort DIRECTLY: eytzinger1_for_each() in-order traversal
 *     must equal the ascending-sorted elements, key and payload - so a
 *     dropped, duplicated, or mis-swapped element is caught independently of
 *     find();
 *   - checks find() against a brute-force linear-search oracle for every
 *     present key plus present-adjacent and random probes.
 *
 * The test body is plain C so it can use the macros/static-inlines directly;
 * a Rust #[test] wrapper (src/eytzinger_test.rs) invokes rust_eytzinger_test()
 * so it runs under `cargo test` with no kernel.
 */

#include <stdio.h>

#include "libbcachefs.h"

#include "fs/util/darray.h"
#include "fs/util/eytzinger.h"

#include "rust_shims.h"

struct eyt_test_elem { u32 key; u32 tag; };

static int eyt_test_cmp(const void *_a, const void *_b)
{
	const struct eyt_test_elem *a = _a, *b = _b;
	return (a->key > b->key) - (a->key < b->key);
}

/* deterministic xorshift so runs are reproducible */
static u32 eyt_rng(u32 *state)
{
	u32 x = *state;
	x ^= x << 13;
	x ^= x >> 17;
	x ^= x << 5;
	return *state = x;
}

#define MAX_N	256
#define TRIALS	16

#define TEST_FAIL(...) ({ fprintf(stderr, "rust_eytzinger_test FAIL: " __VA_ARGS__); fputc('\n', stderr); 1; })

/* n distinct nonzero keys (0 is never a real snapshot id), tag = index */
static void gen_elems(struct eyt_test_elem *ref, unsigned n, u32 *seed)
{
	for (unsigned i = 0; i < n; i++) {
		u32 k;
retry:
		k = eyt_rng(seed);
		if (!k)
			goto retry;
		for (unsigned j = 0; j < i; j++)
			if (ref[j].key == k)
				goto retry;
		ref[i] = (struct eyt_test_elem){ .key = k, .tag = i };
	}
}

/* brute-force oracle */
static const struct eyt_test_elem *ref_find(const struct eyt_test_elem *ref,
					    unsigned n, u32 key)
{
	for (unsigned i = 0; i < n; i++)
		if (ref[i].key == key)
			return &ref[i];
	return NULL;
}

/* ascending insertion sort of a copy, for the direct sort check (n small) */
static void ref_sort(struct eyt_test_elem *r, unsigned n)
{
	for (unsigned i = 1; i < n; i++) {
		struct eyt_test_elem v = r[i];
		unsigned j = i;
		while (j && r[j - 1].key > v.key) {
			r[j] = r[j - 1];
			j--;
		}
		r[j] = v;
	}
}

/*
 * Raw eytzinger1: 1-based, elements at base[1..n], base[0] unused - mirror
 * that with a size n+1 array passed as the base.
 */
static int test_eytzinger1_raw(void)
{
	int fails = 0;
	u32 seed = 0x9e3779b9;
	struct eyt_test_elem a[MAX_N + 1], ref[MAX_N], sorted[MAX_N];

	for (unsigned n = 1; n <= MAX_N; n++)
	for (unsigned trial = 0; trial < TRIALS; trial++) {
		gen_elems(ref, n, &seed);
		for (unsigned i = 0; i < n; i++)
			a[i + 1] = ref[i];

		eytzinger1_sort(a, n, sizeof(a[0]), eyt_test_cmp, NULL);

		/* direct sort check: in-order traversal == ascending sorted */
		memcpy(sorted, ref, n * sizeof(ref[0]));
		ref_sort(sorted, n);
		unsigned pos = 0;
		eytzinger1_for_each(i, n) {
			if (pos >= n)
				fails += TEST_FAIL("raw n=%u: traversal longer than %u", n, n);
			else if (a[i].key != sorted[pos].key || a[i].tag != sorted[pos].tag)
				fails += TEST_FAIL("raw n=%u trial=%u: inorder[%u] = {%u,%u}, want {%u,%u}",
					n, trial, pos, a[i].key, a[i].tag, sorted[pos].key, sorted[pos].tag);
			pos++;
		}
		if (pos != n)
			fails += TEST_FAIL("raw n=%u: traversal visited %u of %u", n, pos, n);

		/* find every present key -> the element carrying it */
		for (unsigned i = 0; i < n; i++) {
			struct eyt_test_elem s = { .key = ref[i].key };
			int idx = eytzinger1_find(a, n, sizeof(a[0]), eyt_test_cmp, &s);
			if (!idx)
				fails += TEST_FAIL("raw n=%u: present key %u not found", n, ref[i].key);
			else if (a[idx].key != ref[i].key || a[idx].tag != ref[i].tag)
				fails += TEST_FAIL("raw n=%u: find(%u) = {%u,%u}, want {%u,%u}",
					n, ref[i].key, a[idx].key, a[idx].tag, ref[i].key, ref[i].tag);
		}

		/* probes vs oracle: present-adjacent and random keys */
		for (unsigned i = 0; i < n; i++)
			for (int d = -1; d <= 1; d += 2) {
				u32 key = ref[i].key + d;
				struct eyt_test_elem s = { .key = key };
				int idx = eytzinger1_find(a, n, sizeof(a[0]), eyt_test_cmp, &s);
				const struct eyt_test_elem *o = ref_find(ref, n, key);
				if (!o != !idx)
					fails += TEST_FAIL("raw n=%u: find(%u) idx=%d, oracle=%s",
						n, key, idx, o ? "present" : "absent");
			}
		for (unsigned p = 0; p < 2 * n; p++) {
			u32 key = eyt_rng(&seed);
			struct eyt_test_elem s = { .key = key };
			int idx = eytzinger1_find(a, n, sizeof(a[0]), eyt_test_cmp, &s);
			const struct eyt_test_elem *o = ref_find(ref, n, key);
			if (!o != !idx)
				fails += TEST_FAIL("raw n=%u: find(%u) idx=%d, oracle=%s",
					n, key, idx, o ? "present" : "absent");
			else if (o && a[idx].key != key)
				fails += TEST_FAIL("raw n=%u: find(%u) returned key %u", n, key, a[idx].key);
		}
	}

	return fails;
}

/*
 * The darray 1-based wrapper exactly as snapshot_id_dying() uses it: sentinel
 * at index 0, real elements at 1..nr-1, sorted and searched with nr - 1.
 */
static int test_darray_eytzinger1(void)
{
	int fails = 0;
	u32 seed = 0x12345678;
	struct eyt_test_elem ref[MAX_N], sorted[MAX_N];

	for (unsigned n = 1; n <= MAX_N; n++)
	for (unsigned trial = 0; trial < TRIALS; trial++) {
		DARRAY(struct eyt_test_elem) d = {};

		if (darray_push(&d, ((struct eyt_test_elem){})))
			return fails + TEST_FAIL("darray n=%u: sentinel push failed", n);

		gen_elems(ref, n, &seed);
		for (unsigned i = 0; i < n; i++)
			if (darray_push(&d, ref[i])) {
				fails += TEST_FAIL("darray n=%u: element push failed", n);
				goto next;
			}

		if (d.nr != n + 1)
			fails += TEST_FAIL("darray n=%u: nr=%zu, expected %u", n, d.nr, n + 1);

		darray_eytzinger1_sort(d, eyt_test_cmp);

		/* direct sort check over the sorted data[1..n] */
		memcpy(sorted, ref, n * sizeof(ref[0]));
		ref_sort(sorted, n);
		unsigned pos = 0;
		eytzinger1_for_each(i, n) {
			if (pos < n &&
			    (d.data[i].key != sorted[pos].key || d.data[i].tag != sorted[pos].tag))
				fails += TEST_FAIL("darray n=%u trial=%u: inorder[%u] = {%u,%u}, want {%u,%u}",
					n, trial, pos, d.data[i].key, d.data[i].tag, sorted[pos].key, sorted[pos].tag);
			pos++;
		}
		if (pos != n)
			fails += TEST_FAIL("darray n=%u: traversal visited %u of %u", n, pos, n);

		for (unsigned i = 0; i < n; i++) {
			struct eyt_test_elem s = { .key = ref[i].key };
			struct eyt_test_elem *e = darray_eytzinger1_find(d, eyt_test_cmp, &s);
			if (!e)
				fails += TEST_FAIL("darray n=%u: present key %u not found", n, ref[i].key);
			else if (e->key != ref[i].key || e->tag != ref[i].tag)
				fails += TEST_FAIL("darray n=%u: find(%u) = {%u,%u}, want {%u,%u}",
					n, ref[i].key, e->key, e->tag, ref[i].key, ref[i].tag);
		}

		for (unsigned p = 0; p < 2 * n; p++) {
			u32 key = eyt_rng(&seed);
			struct eyt_test_elem s = { .key = key };
			struct eyt_test_elem *e = darray_eytzinger1_find(d, eyt_test_cmp, &s);
			const struct eyt_test_elem *o = ref_find(ref, n, key);
			if (!o != !e)
				fails += TEST_FAIL("darray n=%u: find(%u) %s, oracle=%s",
					n, key, e ? "hit" : "miss", o ? "present" : "absent");
			else if (o && (e->key != key || e->tag != o->tag))
				fails += TEST_FAIL("darray n=%u: find(%u) = {%u,%u}, want {%u,%u}",
					n, key, e->key, e->tag, o->key, o->tag);
		}
next:
		darray_exit(&d);
	}

	return fails;
}

int rust_eytzinger_test(void)
{
	int fails = test_eytzinger1_raw() + test_darray_eytzinger1();

	if (!fails)
		fprintf(stderr, "rust_eytzinger_test: all cases passed (n=1..%u, %u trials each)\n",
			MAX_N, TRIALS);
	return fails;
}
