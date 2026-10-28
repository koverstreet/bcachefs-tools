/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_MOVE_H
#define _BCACHEFS_MOVE_H

#include "bcachefs_ioctl.h"
#include "alloc/buckets.h"
#include "btree/bbpos.h"
#include "btree/iter.h"
#include "data/update.h"
#include "data/move_gen.h"
#include "data/moving_gen.h"
#include "data/moving_update_gen.h"

#include "data/move_defs_gen.h"

extern const char * const bch2_data_ops_strs[];


int bch2_move_extent(struct moving_context *,
		     struct move_bucket *,
		     struct bch_inode_opts *,
		     struct data_update_opts *,
		     struct btree_iter *, unsigned, struct bkey_s_c);

int bch2_move_data_phys(struct bch_fs *, unsigned, u64, u64, unsigned,
			struct bch_move_stats *,
			struct write_point_specifier, bool, bool,
			move_pred_fn, void *);

int bch2_evacuate_data(struct moving_context *, unsigned, u64, u64);

int bch2_evacuate_ec_orphan(struct moving_context *, u64, unsigned, u64, u64);

int bch2_evacuate_bucket(struct moving_context *,
			   struct move_bucket *,
			   struct bpos, int,
			   struct data_update_opts);

void bch2_scrub_journal_queue(struct bch_fs *, enum btree_id, unsigned,
			      struct bkey_i *, unsigned, int);
int bch2_scrub_journal(struct bch_fs *, u64 *);
int bch2_scrub_journal_do_repairs(struct bch_fs *);
int bch2_data_job(struct bch_fs *,
		  struct bch_move_stats *,
		  struct bch_ioctl_data *);

void bch2_move_stats_to_text(struct printbuf *, struct bch_move_stats *);
void bch2_move_stats_exit(struct bch_move_stats *, struct bch_fs *);
void bch2_move_stats_init(struct bch_move_stats *, const char *);

void bch2_fs_moving_ctxts_to_text(struct printbuf *, struct bch_fs *);

void bch2_fs_move_exit(struct bch_fs *);
void bch2_fs_move_init(struct bch_fs *);

#endif /* _BCACHEFS_MOVE_H */
