/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_DAMAGE_H
#define _BCACHEFS_DAMAGE_H

#include "sb/errors_types.h"

/*
 * The damage btree: a persistent record of which inodes have been damaged
 * by errors and repairs, one key per damaged inode at (0, inum, snapshot).
 *
 * The in-memory fsck damage accounting (bch2_fsck_damaged) dies with the
 * mount; this is its durable counterpart, written in the same transaction
 * as the repair that does the damage. The value holds the same records
 * the errors superblock section keeps - per bch_sb_error_id, an
 * occurrence count and the time of last occurrence - so a damage key
 * points back at exactly what happened, in the same vocabulary the error
 * counters and fsck use.
 */

int bch2_damage_validate(struct bch_fs *, struct bkey_s_c,
			 const struct bkey_validate_context *);
void bch2_damage_to_text(struct printbuf *, struct bch_fs *, struct bkey_s_c);

#define bch2_bkey_ops_damage ((struct bkey_ops) {	\
	.key_validate	= bch2_damage_validate,		\
	.val_to_text	= bch2_damage_to_text,		\
	.min_val_size	= 16,				\
})

int bch2_inode_has_damage(struct btree_trans *, u64, u32);
int bch2_damage_accumulate(struct btree_trans *, u64, u32, bch_sb_errors_cpu *);
struct bkey_i *bch2_damage_keys_merge(struct btree_trans *, struct bpos,
				      struct bkey_s_c, struct bkey_s_c);
int bch2_damage_delete(struct btree_trans *, u64, u32);
int bch2_damage_record_data_loss(struct btree_trans *, enum btree_id,
				 struct bpos, enum bch_sb_error_id);

int bch2_check_damage(struct bch_fs *);

#endif /* _BCACHEFS_DAMAGE_H */
