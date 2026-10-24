// SPDX-License-Identifier: GPL-2.0

/* erasure coding: open stripes, and the buckets of stripes being made */

#include "bcachefs.h"

#include "alloc/background.h"

#include "btree/iter.h"
#include "btree/locking.h"

#include "data/ec/create.h"
#include "data/ec/trigger.h"

/*
 * Hash table of open stripes:
 * Stripes that are being created or modified are kept in a hash table, so that
 * stripe deletion can skip them.
 *
 * Additionally, we have a hash table for buckets that have stripes being
 * created, to avoid racing with rebalance:
 */

static bool __bch2_bucket_has_new_stripe(struct bch_fs *c, u64 dev_bucket)
{
	unsigned hash = hash_64(dev_bucket, ilog2(ARRAY_SIZE(c->ec.stripes_new_buckets)));
	struct ec_stripe_new_bucket *s;

	hlist_for_each_entry(s, &c->ec.stripes_new_buckets[hash], hash)
		if (s->dev_bucket == dev_bucket)
			return true;
	return false;
}

bool bch2_bucket_has_new_stripe(struct bch_fs *c, u64 dev_bucket)
{
	guard(spinlock)(&c->ec.stripes_new_lock);
	return __bch2_bucket_has_new_stripe(c, dev_bucket);
}

static void stripe_new_bucket_add(struct bch_fs *c, struct ec_stripe_new_bucket *s, u64 dev_bucket)
{
	s->dev_bucket = dev_bucket;

	unsigned hash = hash_64(dev_bucket, ilog2(ARRAY_SIZE(c->ec.stripes_new_buckets)));
	hlist_add_head(&s->hash, &c->ec.stripes_new_buckets[hash]);
}

void bch2_stripe_new_buckets_add(struct bch_fs *c, struct ec_stripe_new *s)
{
	unsigned nr_blocks = s->new_stripe.key.v.nr_blocks;

	guard(spinlock)(&c->ec.stripes_new_lock);
	for (unsigned i = 0; i < nr_blocks; i++) {
		if (!s->blocks[i])
			continue;

		struct open_bucket *ob = c->allocator.open_buckets + s->blocks[i];
		struct bpos bucket = POS(ob->dev, ob->bucket);

		stripe_new_bucket_add(c, &s->buckets[i], bucket_to_u64(bucket));
	}
}

void bch2_stripe_new_buckets_del(struct bch_fs *c, struct ec_stripe_new *s)
{
	guard(spinlock)(&c->ec.stripes_new_lock);

	for (unsigned i = 0; i < s->new_stripe.key.v.nr_blocks; i++)
		if (!hlist_unhashed(&s->buckets[i].hash))
			hlist_del_init(&s->buckets[i].hash);
}

static struct ec_stripe_handle *bch2_open_stripe_find(struct bch_fs *c, u64 idx)
{
	unsigned hash = hash_64(idx, ilog2(ARRAY_SIZE(c->ec.stripes_new)));
	struct ec_stripe_handle *s;

	hlist_for_each_entry(s, &c->ec.stripes_new[hash], hash) {
		/* every handle on the list is claimed, and hashed by its idx: */
		EBUG_ON(!s->idx);
		EBUG_ON(hash_64(s->idx, ilog2(ARRAY_SIZE(c->ec.stripes_new))) != hash);

		if (s->idx == idx)
			return s;
	}
	return NULL;
}

bool bch2_stripe_is_open(struct bch_fs *c, u64 idx)
{
	guard(spinlock)(&c->ec.stripes_new_lock);
	return bch2_open_stripe_find(c, idx) != NULL;
}

bool bch2_stripe_handle_tryget(struct bch_fs *c,
			       struct ec_stripe_handle *s,
			       u64 idx)
{
	BUG_ON(s->idx);
	BUG_ON(!idx);

	guard(spinlock)(&c->ec.stripes_new_lock);
	bool ret = !bch2_open_stripe_find(c, idx);
	if (ret) {
		unsigned hash = hash_64(idx, ilog2(ARRAY_SIZE(c->ec.stripes_new)));

		s->idx = idx;
		hlist_add_head(&s->hash, &c->ec.stripes_new[hash]);
	}
	return ret;
}

/* Intent-locked existing key: 1 acquired, 0 busy, negative error/restart. */
int bch2_stripe_handle_tryget_existing(struct btree_iter *iter,
				       struct ec_stripe_handle *s)
{
	struct btree_trans *trans = iter->trans;
	u64 idx = iter->pos.offset;

	EBUG_ON(iter->btree_id != BTREE_ID_stripes);
	BUG_ON(!btree_node_intent_locked(btree_iter_path(trans, iter), 0));
	BUG_ON(s->idx);

	if (bch2_stripe_is_open(trans->c, idx))
		return 0;

	/*
	 * The stripe trigger and device invalidation act on stripes that
	 * aren't open, and their decision can outlive a lock drop: commit
	 * relocks and retries without re-running triggers. Cycling a write
	 * lock advances the lock sequence, so that relock fails.
	 *
	 * The lock to cycle is the key cache entry: anything updating this key
	 * holds it, and an entry is only freed under its write lock - so if
	 * there's no entry, they're already invalidated. Our leaf intent lock
	 * keeps one from being created meanwhile (the fill write locks the
	 * leaf).
	 */
	CLASS(btree_iter, ck_iter)(trans, BTREE_ID_stripes, iter->pos,
				   BTREE_ITER_intent|
				   BTREE_ITER_cached|
				   BTREE_ITER_cached_nofill);
	try(bch2_btree_iter_traverse(&ck_iter));

	struct btree_path *ck_path = btree_iter_path(trans, &ck_iter);
	if (!ck_path->l[0].b)
		return bch2_stripe_handle_tryget(trans->c, s, idx);

	/* Take the lock before publishing, so a restart can't leak a handle: */
	try(bch2_btree_node_lock_write(trans, ck_path, &ck_path->l[0].b->c));
	int ret = bch2_stripe_handle_tryget(trans->c, s, idx);
	bch2_btree_node_unlock_write(trans, ck_path, ck_path->l[0].b);
	return ret;
}

void bch2_stripe_handle_put(struct bch_fs *c, struct ec_stripe_handle *s)
{
	if (!s->idx)
		return;

	guard(spinlock)(&c->ec.stripes_new_lock);
	BUG_ON(bch2_open_stripe_find(c, s->idx) != s);
	hlist_del_init(&s->hash);

	s->idx = 0;
}

/*
 * Move a claim on a stripe from @src to @dst, which must hold none, without the
 * stripe ever being seen unclaimed.
 */
void bch2_stripe_handle_move(struct bch_fs *c, struct ec_stripe_handle *dst,
			     struct ec_stripe_handle *src)
{
	BUG_ON(dst->idx);

	if (!src->idx)
		return;

	unsigned hash = hash_64(src->idx, ilog2(ARRAY_SIZE(c->ec.stripes_new)));

	guard(spinlock)(&c->ec.stripes_new_lock);
	BUG_ON(bch2_open_stripe_find(c, src->idx) != src);
	hlist_del_init(&src->hash);

	dst->idx = src->idx;
	hlist_add_head(&dst->hash, &c->ec.stripes_new[hash]);

	src->idx = 0;
}
