/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_UTIL_CUCKOO_H
#define _BCACHEFS_UTIL_CUCKOO_H

/*
 * A set of u64s, stored inline: 3-way cuckoo hashing.
 *
 * For sets whose members are just numbers - stripe indexes, bucket numbers -
 * where an rhashtable's per-entry allocation and rhash_head would cost several
 * times the key itself: a member here is a u64 in one flat array.
 *
 * - Lookup checks exactly three slots, plus the stash.
 * - 0 marks an empty slot, so 0 can't be a member.
 * - Insert evicts along a bounded chain; if that fails, the table is rehashed
 *   with new seeds into one sized for three slots per member, growing when a
 *   size keeps failing.
 * - The stash holds the one member a failed insert left homeless, so that a
 *   rehash that can't allocate still loses nothing: add() only fails, with the
 *   set unchanged, when it can't get memory with the stash already occupied.
 * - No locking: the caller serializes.
 *
 * Derived from buckets_waiting_for_journal (removed in 056659eef98b).
 */

#include <linux/hash.h>
#include <linux/types.h>

#define CUCKOO_NR_HASH	3

struct cuckoo_u64 {
	u64		seeds[CUCKOO_NR_HASH];
	unsigned	bits;
	size_t		nr;
	u64		stash;
	u64		*d;
};

static inline u64 *cuckoo_u64_slot(u64 *d, unsigned bits, const u64 *seeds,
				   unsigned i, u64 key)
{
	return d + hash_64(key ^ seeds[i], bits);
}

static inline bool cuckoo_u64_test(const struct cuckoo_u64 *t, u64 key)
{
	if (!key)
		return false;
	if (t->stash == key)
		return true;
	for (unsigned i = 0; i < CUCKOO_NR_HASH; i++)
		if (*cuckoo_u64_slot(t->d, t->bits, t->seeds, i, key) == key)
			return true;
	return false;
}

int cuckoo_u64_add(struct cuckoo_u64 *, u64, gfp_t);
bool cuckoo_u64_del(struct cuckoo_u64 *, u64);
void cuckoo_u64_clear(struct cuckoo_u64 *);

void cuckoo_u64_exit(struct cuckoo_u64 *);
int cuckoo_u64_init(struct cuckoo_u64 *, unsigned, gfp_t);

#endif /* _BCACHEFS_UTIL_CUCKOO_H */
