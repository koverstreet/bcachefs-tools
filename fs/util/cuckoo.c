// SPDX-License-Identifier: GPL-2.0

#include <linux/log2.h>
#include <linux/minmax.h>
#include <linux/random.h>
#include <linux/slab.h>
#include <linux/vmalloc.h>
#include "cuckoo.h"

/* How many evictions an insert chases before the table gets rehashed: */
#define CUCKOO_MAX_EVICTIONS	10

static void cuckoo_u64_seed(u64 *seeds)
{
	for (unsigned i = 0; i < CUCKOO_NR_HASH; i++)
		get_random_bytes(&seeds[i], sizeof(seeds[i]));
}

/*
 * Insert *key, evicting along a chain: returns false if the chain ran out, with
 * *key then holding whichever member was left without a slot - not necessarily
 * the one we started with.
 */
static bool __cuckoo_u64_insert(u64 *d, unsigned bits, const u64 *seeds, u64 *key)
{
	u64 *last_evicted = NULL;

	for (unsigned tries = 0; tries < CUCKOO_MAX_EVICTIONS; tries++) {
		u64 *victim = NULL;

		for (unsigned i = 0; i < CUCKOO_NR_HASH; i++) {
			u64 *slot = cuckoo_u64_slot(d, bits, seeds, i, *key);

			if (!*slot) {
				*slot = *key;
				return true;
			}

			if (slot != last_evicted)
				victim = slot;
		}

		/* every hash function gave the same slot: */
		if (!victim)
			break;

		swap(*key, *victim);
		last_evicted = victim;
	}

	return false;
}

/*
 * Rebuild the table, stash included, into one sized for three slots per
 * member: new seeds, and a bigger table when three sets of seeds at one size
 * won't take everything. On -ENOMEM the table is unchanged.
 */
static int cuckoo_u64_rehash(struct cuckoo_u64 *t, gfp_t gfp)
{
	unsigned bits = max_t(unsigned, t->bits,
			      ilog2(roundup_pow_of_two(max_t(size_t, t->nr, 1) * 3)));

	while (1) {
		u64 *d = kvcalloc(1UL << bits, sizeof(u64), gfp);
		if (!d)
			return -ENOMEM;

		for (unsigned attempt = 0; attempt < 3; attempt++) {
			u64 seeds[CUCKOO_NR_HASH];
			cuckoo_u64_seed(seeds);
			memset(d, 0, sizeof(u64) << bits);

			bool ok = true;
			for (size_t i = 0; ok && i <= 1UL << t->bits; i++) {
				u64 key = i < 1UL << t->bits ? t->d[i] : t->stash;

				ok = !key || __cuckoo_u64_insert(d, bits, seeds, &key);
			}

			if (ok) {
				kvfree(t->d);
				t->d		= d;
				t->bits		= bits;
				t->stash	= 0;
				memcpy(t->seeds, seeds, sizeof(seeds));
				return 0;
			}
		}

		kvfree(d);
		bits++;
	}
}

/*
 * Add @key to the set. Fails only with -ENOMEM, leaving the set unchanged -
 * and only when the stash is already holding a member from an earlier insert
 * whose rehash couldn't allocate.
 */
int cuckoo_u64_add(struct cuckoo_u64 *t, u64 key, gfp_t gfp)
{
	BUG_ON(!key);

	if (cuckoo_u64_test(t, key))
		return 0;

	if (t->stash) {
		int ret = cuckoo_u64_rehash(t, gfp);
		if (ret)
			return ret;
	}

	t->nr++;
	if (__cuckoo_u64_insert(t->d, t->bits, t->seeds, &key))
		return 0;

	/*
	 * The member left over is in the set as far as lookups go, in the
	 * stash; if this rehash can't allocate, the next add() retries it:
	 */
	t->stash = key;
	cuckoo_u64_rehash(t, gfp);
	return 0;
}

bool cuckoo_u64_del(struct cuckoo_u64 *t, u64 key)
{
	if (!key)
		return false;

	if (t->stash == key) {
		t->stash = 0;
		t->nr--;
		return true;
	}

	for (unsigned i = 0; i < CUCKOO_NR_HASH; i++) {
		u64 *slot = cuckoo_u64_slot(t->d, t->bits, t->seeds, i, key);

		if (*slot == key) {
			*slot = 0;
			t->nr--;
			return true;
		}
	}

	return false;
}

void cuckoo_u64_clear(struct cuckoo_u64 *t)
{
	memset(t->d, 0, sizeof(u64) << t->bits);
	t->stash	= 0;
	t->nr		= 0;
}

void cuckoo_u64_exit(struct cuckoo_u64 *t)
{
	kvfree(t->d);
	t->d = NULL;
}

int cuckoo_u64_init(struct cuckoo_u64 *t, unsigned bits, gfp_t gfp)
{
	memset(t, 0, sizeof(*t));
	t->bits	= bits;
	t->d	= kvcalloc(1UL << bits, sizeof(u64), gfp);
	if (!t->d)
		return -ENOMEM;

	cuckoo_u64_seed(t->seeds);
	return 0;
}
