/* SPDX-License-Identifier: GPL-2.0 */

#ifndef _BCACHEFS_DATA_UPDATE_H
#define _BCACHEFS_DATA_UPDATE_H

#include "btree/bkey_buf.h"
#include "btree/update.h"
#include "data/read.h"
#include "data/write_gen.h"

#include "data/update_gen.h"

/* Both scrub types read to check, and must never rewrite what they're checking */
static inline bool data_update_is_scrub(enum bch_data_update_types type)
{
	return type == BCH_DATA_UPDATE_scrub ||
	       type == BCH_DATA_UPDATE_scrub_no_repair;
}

void bch2_data_update_to_text(struct printbuf *, struct data_update *);
void bch2_data_update_inflight_to_text(struct printbuf *, struct data_update *);

void bch2_data_update_read_done(struct data_update *);
bool bch2_data_update_read_err_benign(int);

bool bch2_data_update_fail_should_trace(enum bch_data_update_types, int);

void bch2_data_update_exit(struct data_update *, int);
int bch2_data_update_init(struct btree_trans *, struct btree_iter *,
			  struct moving_context *,
			  struct data_update *,
			  struct write_point_specifier,
			  struct bch_inode_opts *, struct data_update_opts,
			  enum btree_id, struct bkey_s_c);

void bch2_fs_data_update_exit(struct bch_fs *);
int bch2_fs_data_update_init(struct bch_fs *);

#endif /* _BCACHEFS_DATA_UPDATE_H */
