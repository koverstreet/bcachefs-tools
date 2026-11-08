/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_IO_READ_H
#define _BCACHEFS_IO_READ_H

#include "btree/bkey_buf.h"
#include "btree/iter.h"
#include "data/extents_gen.h"
#include "data/reflink_gen.h"

#include "data/read_gen.h"

#define to_rbio(_bio)		container_of((_bio), struct bch_read_bio, bio)


static inline int bch2_read_indirect_extent(struct btree_trans *trans,
					    enum btree_id *data_btree,
					    s64 *offset_into_extent,
					    struct bkey_buf *extent)
{
	if (extent->k->k.type != KEY_TYPE_reflink_p)
		return 0;

	*data_btree = BTREE_ID_reflink;

	struct bch_fs *c = trans->c;
	CLASS(btree_iter_uninit, iter)(trans);
	struct bkey_s_c k = bkey_try(bch2_lookup_indirect_extent(trans, &iter,
						offset_into_extent,
						bkey_i_to_s_c(extent->k),
						true, 0));

	if (bkey_deleted(k.k))
		return bch_err_throw(c, missing_indirect_extent);

	bch2_bkey_buf_reassemble(extent, k);
	return 0;
}

void bch2_read_err_msg_trans(struct btree_trans *, struct printbuf *,
			     struct bch_read_bio *, struct bpos);

enum bch_sb_error_id bch2_data_read_sb_err(int);

int __bch2_read_extent(struct btree_trans *, struct bch_read_bio *,
		       struct bvec_iter, struct bpos, enum btree_id,
		       struct bkey_s_c, unsigned,
		       struct bch_io_failures *, enum bch_read_flags, int);

static inline int bch2_read_extent(struct btree_trans *trans,
			struct bch_read_bio *rbio, struct bpos read_pos,
			enum btree_id data_btree, struct bkey_s_c k,
			unsigned offset_into_extent, enum bch_read_flags flags)
{
	return __bch2_read_extent(trans, rbio, rbio->bio.bi_iter, read_pos,
				     data_btree, k, offset_into_extent, NULL, flags, -1);
}

int bch2_read(struct btree_trans *, struct bch_read_bio *, struct bvec_iter,
	      subvol_inum,
	      struct bch_io_failures *, struct bkey_buf *, enum bch_read_flags);

static inline struct bch_read_bio *rbio_init_fragment(struct bio *bio,
						      struct bch_read_bio *orig,
						      struct bch_io_failures *failed)
{
	struct bch_read_bio *rbio = to_rbio(bio);

	rbio->c			= orig->c;
	rbio->_state		= 0;
	rbio->flags		= 0;
	rbio->ret		= 0;
	rbio->split		= true;
	rbio->parent		= orig;
	rbio->opts		= orig->opts;
	rbio->failed		= failed;
	rbio->err_report	= orig->err_report;
	rbio->list_idx	= 0;
	return rbio;
}

static inline struct bch_read_bio *rbio_init(struct bio *bio,
					     struct bch_fs *c,
					     struct bch_inode_opts opts,
					     bio_end_io_t end_io)
{
	struct bch_read_bio *rbio = to_rbio(bio);

	rbio->start_time	= local_clock();
	rbio->c			= c;
	rbio->_state		= 0;
	rbio->flags		= 0;
	rbio->ret		= 0;
	rbio->failed		= NULL;
	rbio->err_report	= NULL;
	rbio->opts		= opts;
	rbio->bio.bi_end_io	= end_io;
	rbio->list_idx	= 0;
	return rbio;
}

void bch2_read_bio_to_text(struct printbuf *, struct bch_fs *, struct bch_read_bio *);

#endif /* _BCACHEFS_IO_READ_H */
