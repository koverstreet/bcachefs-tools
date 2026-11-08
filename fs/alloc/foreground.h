/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_ALLOC_FOREGROUND_H
#define _BCACHEFS_ALLOC_FOREGROUND_H

#include "bcachefs.h"
#include "alloc/buckets.h"
#include "alloc/types_gen.h"
#include "btree/iter.h"
#include "data/extents.h"
#include "data/write_gen.h"
#include "sb/members.h"

#include <linux/hash.h>

#include "alloc/foreground_gen.h"

static inline struct bch_dev *ob_dev(struct bch_fs *c, struct open_bucket *ob)
{
	return bch2_dev_have_ref(c, ob->dev);
}

static inline void ob_push(struct bch_fs *c, struct open_buckets *obs,
			   struct open_bucket *ob)
{
	BUG_ON(obs->nr >= ARRAY_SIZE(obs->v));

	obs->v[obs->nr++] = ob - c->allocator.open_buckets;
}

#define open_bucket_for_each(_c, _obs, _ob, _i)					\
	for ((_i) = 0;								\
	     (_i) < (_obs)->nr &&						\
	     ((_ob) = (_c)->allocator.open_buckets + (_obs)->v[_i], true);	\
	     (_i)++)

static inline void bch2_open_bucket_put(struct bch_fs *c, struct open_bucket *ob)
{
	if (atomic_dec_and_test(&ob->pin))
		__bch2_open_bucket_put(c, ob);
}

static inline void bch2_open_bucket_get(struct bch_fs *c,
					struct write_point *wp,
					struct open_buckets *ptrs)
{
	struct open_bucket *ob;
	unsigned i;

	open_bucket_for_each(c, &wp->ptrs, ob, i) {
		ob->data_type = wp->data_type;
		atomic_inc(&ob->pin);
		ob_push(c, ptrs, ob);
	}
}

static inline open_bucket_idx_t *open_bucket_hashslot(struct bch_fs *c,
						  unsigned dev, u64 bucket)
{
	return c->allocator.open_buckets_hash + bch2_open_bucket_hash(dev, bucket);
}

static inline struct open_bucket *bch2_bucket_is_open(struct bch_fs *c, unsigned dev, u64 bucket)
{
	open_bucket_idx_t slot = *open_bucket_hashslot(c, dev, bucket);

	while (slot) {
		struct open_bucket *ob = &c->allocator.open_buckets[slot];

		if (ob->dev == dev && ob->bucket == bucket)
			return ob;

		slot = ob->hash;
	}

	return NULL;
}



DEFINE_FREE(alloc_request_put, struct alloc_request *,
	    if (!IS_ERR_OR_NULL(_T)) alloc_request_put(_T))

static inline struct write_point_specifier writepoint_hashed(unsigned long v)
{
	return (struct write_point_specifier) { .v = v | 1 };
}

static inline struct write_point_specifier writepoint_ptr(struct write_point *wp)
{
	return (struct write_point_specifier) { .v = (unsigned long) wp };
}


/*
 * Returns an error if the filesystem has gone emergency read-only: the
 * allocator won't hand out buckets again, so callers must not retry. A normal
 * read-only transition still allocates (for the final flush), and keeps waiting.
 */
static inline int bch2_wait_on_allocator(struct btree_trans *trans,
					 struct alloc_request *req,
					 int err,
					 struct closure *cl)
{
	if (closure_nr_remaining(cl) > 1)
		__bch2_wait_on_allocator(trans, req, err, cl);

	return test_bit(BCH_FS_emergency_ro, &trans->c->flags)
		? bch_err_throw(trans->c, emergency_ro)
		: 0;
}

#endif /* _BCACHEFS_ALLOC_FOREGROUND_H */
