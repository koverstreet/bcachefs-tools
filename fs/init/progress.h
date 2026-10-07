/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_PROGRESS_H
#define _BCACHEFS_PROGRESS_H

#include "bcachefs_ioctl.h"
#include "btree/bbpos_types.h"

#include "init/progress_types.h"

void bch2_progress_init(struct progress_indicator *s,
			      const char *msg,
			      struct bch_fs *c,
			      u64 leaf_btree_id_mask,
			      u64 inner_btree_id_mask);

/*
 * For work that isn't a btree walk: no node to count and no position to report,
 * so the caller supplies the total and counts its own units. Journal replay
 * goes through a sorted array of keys.
 */
void bch2_progress_init_count(struct progress_indicator *s,
			      const char *msg,
			      enum bch_progress_units units,
			      u64 total);

int bch2_progress_update_iter(struct btree_trans *,
			      struct progress_indicator *,
			      struct btree_iter *);

void bch2_progress_update_count(struct bch_fs *, struct progress_indicator *);

void bch2_progress_to_text(struct printbuf *, struct progress_indicator *);

#endif /* _BCACHEFS_PROGRESS_H */
