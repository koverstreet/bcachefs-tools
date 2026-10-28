/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_MOVE_H
#define _BCACHEFS_MOVE_H

#include "bcachefs_ioctl.h"
#include "alloc/buckets.h"
#include "btree/bbpos.h"
#include "btree/iter.h"
#include "data/update.h"
#include "data/move_gen.h"
#include "data/move_data_gen.h"
#include "data/moving_gen.h"
#include "data/moving_update_gen.h"

#include "data/move_defs_gen.h"

void bch2_scrub_journal_queue(struct bch_fs *, enum btree_id, unsigned,
			      struct bkey_i *, unsigned, int);
int bch2_scrub_journal(struct bch_fs *, u64 *);
int bch2_scrub_journal_do_repairs(struct bch_fs *);

void bch2_move_stats_init(struct bch_move_stats *, const char *);

void bch2_fs_move_exit(struct bch_fs *);
void bch2_fs_move_init(struct bch_fs *);

#endif /* _BCACHEFS_MOVE_H */
