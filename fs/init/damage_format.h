/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_DAMAGE_FORMAT_H
#define _BCACHEFS_DAMAGE_FORMAT_H

#include "sb/errors_format.h"

/*
 * Damage tracking: one key per damaged inode at (0, inum, snapshot),
 * recording which errors damaged it. Written in the same transaction as
 * the repair that did the damage, so the record can't be lost to a crash
 * between repair and bookkeeping.
 *
 * The value is the same records the errors superblock section keeps,
 * sorted by error id: bch_sb_field_error_entry_v2 packs the id, a
 * saturating occurrence count and the times of first and last
 * occurrence (BCH_SB_ERROR_ENTRY_V2_ID/NR/FIRST/LAST). One vocabulary
 * for "what happened": the sb section counts per-filesystem, damage
 * keys count per-inode.
 */
struct bch_damage {
	struct bch_val		v;
	bch_sb_field_error_entry_v2 entries[];
};

#endif /* _BCACHEFS_DAMAGE_FORMAT_H */
