/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_ALLOC_CHECK_DATA_H
#define _BCACHEFS_ALLOC_CHECK_DATA_H

int bch2_drop_dev_and_update(struct btree_trans *, enum btree_id, unsigned,
			     struct bkey_s_c, unsigned);
int bch2_kill_replica_if_checksum_bad(struct btree_trans *, enum btree_id,
				      unsigned, struct bkey_s_c, enum btree_id,
				      struct bkey_s_c, unsigned);

int bch2_check_fix_ptrs(struct btree_trans *, struct btree_iter *,
			enum btree_id, unsigned, struct bkey_s_c);

#endif /* _BCACHEFS_ALLOC_CHECK_DATA_H */
