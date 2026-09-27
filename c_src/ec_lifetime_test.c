// SPDX-License-Identifier: GPL-2.0
/* Userspace regression: real stripe trigger, update paths and optimistic locks. */
#include <stdio.h>

#include "libbcachefs.h"
#include "fs/btree/bkey_buf.h"
#include "fs/btree/locking.h"
#include "fs/btree/update.h"
#include "fs/data/ec/create.h"
#include "fs/data/ec/trigger.h"
#include "fs/init/fs.h"

int rust_test_ec_lifetime(const char **, unsigned);

#define check(_cond) do {                                                \
	if (!(_cond)) {                                                   \
		fprintf(stderr, "%s:%u: %s\n", __func__, __LINE__, #_cond); \
		return -EINVAL;                                           \
	}                                                                \
} while (0)

struct test_handle {
	struct bch_fs *c;
	struct ec_stripe_handle h;
};

static void test_handle_put(struct test_handle *s)
{
	bch2_stripe_handle_put(s->c, &s->h);
}

static int first_stripe(struct btree_trans *trans, struct bkey_buf *saved)
{
	struct bkey_s_c k;
	int ret;

	for_each_btree_key_norestart(trans, iter, BTREE_ID_stripes, POS_MIN, 0, k, ret) {
		if (k.k->type != KEY_TYPE_stripe)
			continue;
		check(stripe_lru_pos(bkey_s_c_to_stripe(k).v) != STRIPE_LRU_POS_EMPTY);
		bch2_bkey_buf_reassemble(saved, k);
		return 0;
	}
	return ret ?: -ENOENT;
}

/*
 * Stage the exact live -> empty stripe trigger transition. The fixture's
 * extents remain live: these updates MUST be aborted, never committed. This
 * tests deletion staging/relocking, not a complete last-file unlink.
 */
static int stage_empty(struct btree_trans *trans, struct bpos pos)
{
	/* Extent accounting updates stripes through an explicit cached iterator. */
	CLASS(btree_iter, iter)(trans, BTREE_ID_stripes, pos,
		BTREE_ITER_intent | BTREE_ITER_cached);
	struct bkey_s_c k = bkey_try(bch2_btree_iter_peek_slot(&iter));
	check(k.k->type == KEY_TYPE_stripe);
	struct bkey_buf old __cleanup(bch2_bkey_buf_exit);
	bch2_bkey_buf_init(&old);
	bch2_bkey_buf_reassemble(&old, k);
	struct bkey_i_stripe *new =
		errptr_try(bch2_bkey_make_mut_typed(trans, &iter, &k, 0, stripe));

	for (unsigned i = 0; i < new->v.nr_blocks - new->v.nr_redundant; i++)
		stripe_blockcount_set(&new->v, i, 0);
	check(stripe_lru_pos(&new->v) == STRIPE_LRU_POS_EMPTY);
	return bch2_trigger_stripe(trans, (struct btree_trigger_op) {
		.btree = BTREE_ID_stripes,
		.old = bkey_i_to_s_c(old.k),
		.new = bkey_i_to_s(&new->k_i),
		.flags = BTREE_TRIGGER_transactional,
	});
}

static int check_updates(struct btree_trans *trans, struct bpos pos, bool deleted)
{
	unsigned cached = 0, leaf = 0, live = 0;

	for (unsigned i = 0; i < trans->nr_updates; i++) {
		struct btree_insert_entry *u = trans->updates + i;
		if (u->btree_id != BTREE_ID_stripes || !bpos_eq(u->k->k.p, pos))
			continue;
		if (u->k->k.type == KEY_TYPE_deleted) {
			check(trans->paths[u->path].ref);
			check(trans->paths[u->path].should_be_locked);
			if (u->cached) {
				check(u->key_cache_already_flushed);
				cached++;
			} else {
				leaf++;
			}
		} else if (u->k->k.type == KEY_TYPE_stripe) {
			live++;
		}
	}
	check(deleted ? (cached == 1 && leaf == 1 && !live)
		      : (!cached && !leaf && live == 1));
	return 0;
}

enum test_path { TEST_COLD, TEST_OVERLAY, TEST_CACHED };

static int acquire(struct btree_trans *trans, struct test_handle *owner,
		   struct bpos pos, enum test_path mode)
{
	CLASS(btree_iter, iter)(trans, BTREE_ID_stripes, pos,
		BTREE_ITER_intent | BTREE_ITER_nopreserve |
		(mode == TEST_CACHED ? BTREE_ITER_cached : 0));
	struct bkey_s_c k = bkey_try(bch2_btree_iter_peek_slot(&iter));
	check(k.k->type == KEY_TYPE_stripe);
	/* This verifies both production callers' effective path shapes. */
	if (mode == TEST_CACHED)
		check(btree_iter_path(trans, &iter)->cached);
	else
		check(!btree_iter_path(trans, &iter)->cached &&
		      !!iter.key_cache_path == (mode == TEST_OVERLAY));

	int ret = bch2_stripe_handle_tryget_existing(&iter, &owner->h);
	if (ret < 0)
		return ret;
	check(ret == 1 && owner->h.idx == pos.offset);
	struct test_handle competitor __cleanup(test_handle_put) = { .c = trans->c };
	check(bch2_stripe_handle_tryget_existing(&iter, &competitor.h) == 0);
	check(!competitor.h.idx);
	return 0;
}

static int unchanged(struct btree_trans *trans, const struct bkey_buf *before)
{
	CLASS(btree_iter, iter)(trans, BTREE_ID_stripes, before->k->k.p, 0);
	struct bkey_s_c k = bkey_try(bch2_btree_iter_peek_slot(&iter));
	struct bkey_buf after __cleanup(bch2_bkey_buf_exit);
	bch2_bkey_buf_init(&after);
	bch2_bkey_buf_reassemble(&after, k);
	check(after.k->k.u64s == before->k->k.u64s);
	check(!memcmp(after.k, before->k, bkey_bytes(&before->k->k)));
	return 0;
}

static int run_case(struct bch_fs *c, const struct bkey_buf *before, bool cached)
{
	struct bpos pos = before->k->k.p;
	struct test_handle owner __cleanup(test_handle_put) = { .c = c };
	CLASS(btree_trans, deletion)(c);
	try(lockrestart_do(deletion, stage_empty(deletion, pos)));
	try(check_updates(deletion, pos, true));
	bch2_trans_unlock(deletion);

	/* Two real transactions, deterministically interleaved on one worker. */
	{
		CLASS(btree_trans, opener)(c);
		check(opener != deletion);
		try(lockrestart_do(opener, acquire(opener, &owner, pos,
				cached ? TEST_CACHED : TEST_OVERLAY)));
	}
	int ret = bch2_trans_relock_notrace(deletion);
	fprintf(stderr, "ec lifetime: %s relock=%d\n", cached ? "cached" : "overlay", ret);
	/* Publishing the handle must invalidate the deletion's optimistic relock: */
	bool expected = bch2_err_matches(ret, BCH_ERR_transaction_restart);

	/* A restarted trigger observes the owner and leaves the stripe alive. */
	bch2_trans_reset_updates(deletion);
	bch2_trans_begin(deletion);
	check(expected);
	try(lockrestart_do(deletion, stage_empty(deletion, pos)));
	try(check_updates(deletion, pos, false));
	bch2_stripe_handle_put(c, &owner.h);
	check(!bch2_stripe_is_open(c, pos.offset));
	bch2_trans_reset_updates(deletion);
	try(lockrestart_do(deletion, stage_empty(deletion, pos)));
	try(check_updates(deletion, pos, true));
	/* Explicitly abort every staged update; no synthetic transition reaches disk. */
	bch2_trans_reset_updates(deletion);
	try(lockrestart_do(deletion, unchanged(deletion, before)));
	return 0;
}

static int observe_closed(struct btree_trans *trans, struct bpos pos)
{
	CLASS(btree_iter, iter)(trans, BTREE_ID_stripes, pos, BTREE_ITER_intent);
	struct bkey_s_c k = bkey_try(bch2_btree_iter_peek_slot(&iter));
	check(k.k->type == KEY_TYPE_stripe);
	check(!iter.key_cache_path);
	check(!bch2_stripe_is_open(trans->c, pos.offset));
	check(btree_iter_path(trans, &iter)->should_be_locked);
	return 0;
}

/* A guard read before the stripe is in the key cache must also restart. */
static int run_cold_case(struct bch_fs *c, const struct bkey_buf *before)
{
	struct bpos pos = before->k->k.p;
	struct test_handle owner __cleanup(test_handle_put) = { .c = c };
	CLASS(btree_trans, observer)(c);
	try(lockrestart_do(observer, observe_closed(observer, pos)));
	bch2_trans_unlock(observer);
	{
		CLASS(btree_trans, opener)(c);
		try(lockrestart_do(opener, acquire(opener, &owner, pos, TEST_COLD)));
	}
	int ret = bch2_trans_relock_notrace(observer);
	fprintf(stderr, "ec lifetime: cold relock=%d\n", ret);
	bool expected = bch2_err_matches(ret, BCH_ERR_transaction_restart);
	bch2_trans_begin(observer);
	check(expected);
	try(lockrestart_do(observer, unchanged(observer, before)));
	return 0;
}

static int run_matrix(struct bch_fs *c)
{
	struct bkey_buf before __cleanup(bch2_bkey_buf_exit);
	bch2_bkey_buf_init(&before);
	{
		CLASS(btree_trans, trans)(c);
		try(lockrestart_do(trans, first_stripe(trans, &before)));
	}
	try(run_cold_case(c, &before));
	for (unsigned cached = 0; cached < 2; cached++)
		try(run_case(c, &before, cached));
	return 0;
}

int rust_test_ec_lifetime(const char **paths, unsigned nr)
{
	darray_const_str devs = { .data = paths, .nr = nr };
	struct bch_opts opts = bch2_opts_empty();
	/* Queued updates are tested in memory and never need write access. */
	opt_set(opts, read_only, true);
	opt_set(opts, copygc_enabled, false);
	opt_set(opts, reconcile_enabled, false);
	opt_set(opts, auto_snapshot_deletion, false);
	struct bch_fs *c = bch2_fs_open(&devs, &opts, NULL);
	if (IS_ERR(c))
		return PTR_ERR(c);
	int ret = run_matrix(c);
	int exit_ret = bch2_fs_exit(c);
	return ret ?: exit_ret;
}
