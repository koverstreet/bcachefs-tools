/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_DATA_EC_IO_H
#define _BCACHEFS_DATA_EC_IO_H

struct ec_bio {
	struct bch_dev		*ca;
	struct ec_stripe_buf	*buf;
	size_t			idx;
	int			rw;
	u64			submit_time;
	struct bio		bio;
};

enum bch_stripe_buf_err {
	STRIPE_BUF_PRE_RECOV,
	STRIPE_BUF_POST_RECOV,
};

struct ec_stripe_buf {
	struct closure		io;
	struct bch_fs		*c;

	/* might not be buffering the entire stripe: */
	unsigned		offset;
	unsigned		size;
	s16			err[2][BCH_BKEY_PTRS_MAX];
	void			*data[BCH_BKEY_PTRS_MAX];

	/* Stale when we read the stripe key, i.e. alloc inconsistency */
	unsigned long		stale[BITS_TO_LONGS(BCH_BKEY_PTRS_MAX)];

	struct bch_csum		csum_good[BCH_BKEY_PTRS_MAX];
	struct bch_csum		csum_bad[BCH_BKEY_PTRS_MAX];

	struct bkey_i_stripe	key;
	u64			pad[255];
};

static inline unsigned ec_nr_failed(struct ec_stripe_buf *buf,
				    enum bch_stripe_buf_err e)
{
	struct bch_stripe *v = &buf->key.v;

	unsigned nr_failed = 0;
	for (unsigned i = 0; i < v->nr_blocks; i++)
		nr_failed += buf->err[e][i] != 0;
	return nr_failed;
}

static inline u32 ec_failed_mask(struct ec_stripe_buf *buf,
				 enum bch_stripe_buf_err e)
{
	struct bch_stripe *v = &buf->key.v;

	u32 mask = 0;
	for (unsigned i = 0; i < v->nr_blocks; i++)
		if (buf->err[e][i])
			mask |= BIT(i);
	return mask;
}

/*
 * Blocks a stripe read has to come back good, as a mask. Every block is read -
 * a bad block is reconstructed from all the others, so they are all needed as
 * reconstruction inputs, including ones holding no live data. But only the
 * blocks the caller is actually going to consume have to end up valid: damage
 * confined to the rest is not this read's problem.
 *
 * EC_BLOCKS_ALL is the conservative value - every block must be good.
 */
#define EC_BLOCKS_ALL	(~0U)

void bch2_ec_stripe_buf_exit(struct ec_stripe_buf *);
int bch2_ec_stripe_buf_init(struct bch_fs *, struct ec_stripe_buf *, unsigned, unsigned,
			    struct closure *);

DEFINE_FREE(ec_stripe_buf_free, struct ec_stripe_buf *, bch2_ec_stripe_buf_exit(_T); kfree(_T));

void bch2_ec_generate_ec(struct ec_stripe_buf *);
void bch2_ec_generate_checksums(struct ec_stripe_buf *);

int bch2_stripe_buf_validate_msg(struct bch_fs *, struct ec_stripe_buf *, bool, u32);

void bch2_ec_block_io(struct bch_fs *, struct ec_stripe_buf *, blk_opf_t, unsigned);
void bch2_ec_block_io_range(struct bch_fs *, struct ec_stripe_buf *, blk_opf_t, unsigned,
			    unsigned, unsigned);
void bch2_stripe_buf_read(struct bch_fs *, struct ec_stripe_buf *);

struct bch_read_bio;
int bch2_ec_read_extent(struct btree_trans *, struct bch_read_bio *,
			struct bkey_s_c, struct printbuf *);

#endif /* _BCACHEFS_DATA_EC_IO_H */

