/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_DATA_EC_CREATE_H
#define _BCACHEFS_DATA_EC_CREATE_H

#include "io.h"
#include "util/darray.h"

#include "data/ec/create_types.h"

extern const char * const bch2_ec_stripe_new_states[];

/*
 * Geometry lives entirely in the on-disk bkey; ec_stripe_new derives
 * the nr_data/nr_parity split from it.
 */
static inline unsigned ec_stripe_new_nr_data(const struct ec_stripe_new *s)
{
	return s->new_stripe.key.v.nr_blocks - s->new_stripe.key.v.nr_redundant;
}

static inline unsigned ec_stripe_new_nr_parity(const struct ec_stripe_new *s)
{
	return s->new_stripe.key.v.nr_redundant;
}

/*
 * Stripe block layout: data slots at [0, nr_data), parity slots at
 * [nr_data, nr_data + nr_parity). These macros name the ranges for
 * readers; pass nr_data/nr_parity directly so they work for both
 * struct ec_stripe_new (via accessors) and struct bch_stripe
 * (nr_blocks - nr_redundant, nr_redundant).
 */
#define for_each_data_block(_i, _nr_data)				\
	for (unsigned _i = 0; _i < (_nr_data); _i++)

#define for_each_parity_block(_i, _nr_data, _nr_parity)			\
	for (unsigned _i = (_nr_data); _i < (_nr_data) + (_nr_parity); _i++)

#define for_each_data_parity_block(_i, _nr_data, _nr_parity)		\
	for (unsigned _i = 0; _i < (_nr_data) + (_nr_parity); _i++)

void *bch2_writepoint_ec_buf(struct bch_fs *, struct write_point *);

unsigned bch2_disk_label_ec_devs(struct bch_fs *, unsigned, struct bch_devs_mask *, unsigned);
void bch2_disk_label_ec_rw_member_devs(struct bch_fs *, unsigned,
				       struct bch_devs_mask *, unsigned);

bool bch2_can_form_ec_stripe(struct bch_fs *, unsigned, unsigned, struct printbuf *);

int bch2_widen_cache_init(widen_cache *);
int bch2_widen_cache_lookup(widen_cache *, struct bch_fs *,
			    u8 disk_label, u16 sectors, unsigned *nr_devs);

void bch2_ec_stripe_new_cancel(struct bch_fs *, struct ec_stripe_head *,
			       struct ec_stripe_new *, int);
void bch2_ec_bucket_cancel(struct bch_fs *, struct open_bucket *, int);

int bch2_ec_stripe_new_alloc(struct bch_fs *, struct ec_stripe_head *);

void bch2_ec_stripe_head_put(struct bch_fs *, struct ec_stripe_head *);

struct ec_stripe_head *bch2_ec_stripe_head_get(struct btree_trans *,
			struct alloc_request *, unsigned, struct open_bucket **);

void bch2_do_stripe_deletes(struct bch_fs *);
void bch2_ec_stripe_create_start(struct bch_fs *, struct ec_stripe_new *);
void bch2_ec_stripe_new_free(struct bch_fs *, struct ec_stripe_new *);

static inline void ec_stripe_new_get(struct ec_stripe_new *s,
				     enum ec_stripe_ref ref)
{
	atomic_inc(&s->ref[ref]);
}

static inline void ec_stripe_new_put(struct bch_fs *c, struct ec_stripe_new *s,
				     enum ec_stripe_ref ref)
{
	BUG_ON(atomic_read(&s->ref[ref]) <= 0);

	if (atomic_dec_and_test(&s->ref[ref]))
		switch (ref) {
		case STRIPE_REF_stripe:
			bch2_ec_stripe_new_free(c, s);
			break;
		case STRIPE_REF_io:
			/*
			 * Every bucket is back: the data is all in, and creation
			 * is about to be queued. This is the filling -> in_flight
			 * transition, and the point after which the stripe
			 * completes without needing anything from a writer.
			 *
			 * seq is the commit-ready marker, assigned here;
			 * bch2_fs_ec_flush_outstanding() waits on it.
			 */
			s->state = EC_STRIPE_NEW_in_flight;
			s->seq = atomic64_inc_return(&c->ec.stripe_new_seq);
			wake_up(&c->ec.stripe_new_wait);
			bch2_ec_stripe_create_start(c, s);
			break;
		default:
			BUG();
		}
}

void bch2_ec_stripe_delete_work(struct work_struct *);

void bch2_new_stripes_to_text(struct printbuf *, struct bch_fs *);

int bch2_stripe_repair(struct moving_context *, struct btree_iter *, struct bkey_s_c_stripe);
int bch2_stripe_repair_damaged(struct moving_context *, struct btree_iter *,
			       struct bkey_s_c_stripe, unsigned);

void bch2_ec_record_lost_blocks(struct btree_trans *, struct bkey_s_c_stripe, u32,
				enum bch_sb_error_id, bool, const struct ec_stripe_buf *);

void bch2_logged_op_stripe_update_to_text(struct printbuf *, struct bch_fs *, struct bkey_s_c);

#define bch2_bkey_ops_logged_op_stripe_update ((struct bkey_ops) {	\
	.val_to_text	= bch2_logged_op_stripe_update_to_text,		\
	.min_val_size	= 40,						\
})

int bch2_resume_logged_op_stripe_update(struct btree_trans *, struct bkey_i *);

#endif /* _BCACHEFS_DATA_EC_CREATE_H */
