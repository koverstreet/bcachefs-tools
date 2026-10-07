/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_DATA_EXTENTS_DEFS_H
#define _BCACHEFS_DATA_EXTENTS_DEFS_H

#include "enum_kind.h"

struct bch_fs;

struct btree_trans;

union bch_extent_crc {
	u8				type;
	struct bch_extent_crc32		crc32;
	struct bch_extent_crc64		crc64;
	struct bch_extent_crc128	crc128;
} __aligned(8);

/* bkey_ptrs: generically over any key type that has ptrs */

struct bkey_ptrs_c {
	const union bch_extent_entry	*start;
	const union bch_extent_entry	*end;
};

struct bkey_ptrs {
	union bch_extent_entry	*start;
	union bch_extent_entry	*end;
};

/*
 * Everything one walk of a key's pointer list can say about its replication.
 *
 * Gathered together because the walk is the expensive part - and for the exact
 * version, a stripe read per erasure coded pointer - while callers routinely
 * want several of these at once. bch2_sum_sector_overwrites() asks four
 * separate single-value helpers for them, on the same two keys, three of the
 * calls inside a loop.
 *
 * The durability counts are weighted by each device's mi.durability and skip
 * BCH_SB_MEMBER_INVALID placeholders - those are added by
 * bch2_bkey_set_needs_reconcile() on a degraded write, to stand for a replica
 * that isn't there. The raw counts below are unweighted.
 *
 * u8 except the sector count: BCH_MEMBER_DURABILITY is a two bit field and
 * BCH_REPLICAS_MAX is 4, so none of these can come near 255.
 *
 * Not handled here: KEY_TYPE_reservation, which several of the older
 * single-value helpers report as v->nr_replicas. Callers that need that still
 * special-case it themselves.
 */
struct bkey_durability {
	u8		online, total;
	u8		acct, min_durability;

	u8		nr_ptrs;		/* real device pointers */
	u8		nr_overwritable;	/* uncompressed - an overwrite reclaims these */
	unsigned	sectors_compressed;

	/*
	 * Copies this key occupies, for disk space accounting.
	 *
	 * Deliberately not weighted by mi.durability, unlike the counts above:
	 * durability is OPT_RUNTIME, and accounting is persistent, so a
	 * durability change would retroactively invalidate space already
	 * accounted for. This has to be a function of what is physically on
	 * disk.
	 *
	 * Differs from nr_ptrs only for a reservation, which occupies the space
	 * it reserved while having no pointers at all.
	 */
	u8		nr_replicas;

	/*
	 * Copies for the purpose of "does this write increase replication" -
	 * erasure coding counts, because a stripe genuinely provides it.
	 *
	 * Distinct from nr_replicas on purpose: parity is accounted separately
	 * as BCH_DATA_parity at the stripe, so counting redundancy in a
	 * per-extent space figure would charge the same parity to every extent
	 * sharing the stripe. Space and replication are different questions.
	 */
	u8		replicas;
};

/* Generic extent code: */

enum __enum_closed bch_extent_overlap {
	BCH_EXTENT_OVERLAP_ALL		= 0,
	BCH_EXTENT_OVERLAP_BACK		= 1,
	BCH_EXTENT_OVERLAP_FRONT	= 2,
	BCH_EXTENT_OVERLAP_MIDDLE	= 3,
};

#endif /* _BCACHEFS_DATA_EXTENTS_DEFS_H */
