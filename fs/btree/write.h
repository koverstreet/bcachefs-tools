/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_BTREE_WRITE_H
#define _BCACHEFS_BTREE_WRITE_H

#include "data/write_gen.h"

#include "btree/write_gen.h"

bool bch2_btree_post_write_cleanup(struct bch_fs *, struct btree *);

#define BTREE_WRITE_only_if_need	BIT(__BTREE_WRITE_only_if_need)
#define BTREE_WRITE_already_started	BIT(__BTREE_WRITE_already_started)

void __bch2_btree_node_write(struct btree_trans *, struct btree *, unsigned);
void bch2_trans_submit_write_bios(struct btree_trans *);
void bch2_btree_node_write_trans(struct btree_trans *, struct btree *,
				 enum six_lock_type, unsigned);
void bch2_btree_init_next(struct btree_trans *, struct btree *);

static inline void btree_node_write_if_need(struct btree_trans *trans, struct btree *b,
					    enum six_lock_type lock_held)
{
	bch2_btree_node_write_trans(trans, b, lock_held, BTREE_WRITE_only_if_need);
}

void bch2_btree_write_stats_to_text(struct printbuf *, struct bch_fs *);
bool bch2_btree_flush_all_writes(struct bch_fs *);
void bch2_btree_cancel_all_writes(struct bch_fs *);

#endif /* _BCACHEFS_BTREE_WRITE_H */
