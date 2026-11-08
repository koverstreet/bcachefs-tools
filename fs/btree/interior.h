/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_BTREE_INTERIOR_H
#define _BCACHEFS_BTREE_INTERIOR_H

#include "btree/cache.h"
#include "btree/locking.h"
#include "btree/update.h"
#include "data/write_gen.h"

#include "btree/interior_defs_gen.h"
#include "btree/interior_c_gen.h"
#include "btree/interior/split_merge_gen.h"
#include "btree/shard_gen.h"

static inline enum bch_trans_commit_flags
btree_update_set_watermark_hipri(enum bch_trans_commit_flags flags)
{
	enum bch_watermark watermark = flags & BCH_WATERMARK_MASK;
	if (watermark == BCH_WATERMARK_copygc)
		watermark = BCH_WATERMARK_btree_copygc;
	if (watermark < BCH_WATERMARK_btree)
		watermark = BCH_WATERMARK_btree;

	flags &= ~BCH_WATERMARK_MASK;
	flags |= watermark;
	return flags;
}

struct btree *__bch2_btree_node_alloc_replacement(struct btree_update *,
						  struct btree_trans *,
						  struct btree *,
						  struct bkey_format);

static inline bool btree_node_needs_merge(struct bch_fs *c, struct btree *b, int d)
{
	if (static_branch_unlikely(&bch2_btree_node_merging_disabled))
		return false;

	return (int) min(b->sib_u64s[0], b->sib_u64s[1]) + d <=
		(int) c->btree.foreground_merge_threshold;
}

static inline int bch2_foreground_maybe_merge(struct btree_trans *trans,
					      btree_path_idx_t path_idx,
					      unsigned level, enum bch_trans_commit_flags flags,
					      int u64s_delta,
					      u64 *merge_count)
{
	bch2_trans_verify_not_unlocked_or_in_restart(trans);

	struct btree_path *path = trans->paths + path_idx;
	struct btree *b = path->l[level].b;

	EBUG_ON(!btree_node_locked(path, level));

	if (likely(!btree_node_needs_merge(trans->c, b, u64s_delta)))
		return 0;

	return __bch2_foreground_maybe_merge(trans, path_idx, level, flags, merge_count);
}

void bch2_btree_set_root_for_read(struct bch_fs *, struct btree *);

int bch2_btree_root_alloc_fake_trans(struct btree_trans *, enum btree_id, unsigned);

static inline unsigned btree_update_reserve_required(struct bch_fs *c,
						     struct btree *b)
{
	unsigned depth = btree_node_root(c, b)->c.level + 1;

	/*
	 * Number of nodes we might have to allocate in a worst case btree
	 * split operation - we split all the way up to the root, then allocate
	 * a new root, unless we're already at max depth:
	 */
	if (depth < BTREE_MAX_DEPTH)
		return (depth - b->c.level) * 2 + 1;
	else
		return (depth - b->c.level) * 2 - 1;
}

static inline void *btree_data_end(struct btree *b)
{
	return (void *) b->data + btree_buf_bytes(b);
}

static inline struct bkey_packed *unwritten_whiteouts_start(struct btree *b)
{
	return (void *) ((u64 *) btree_data_end(b) - b->whiteout_u64s);
}

static inline struct bkey_packed *unwritten_whiteouts_end(struct btree *b)
{
	return btree_data_end(b);
}

static inline void *write_block(struct btree *b)
{
	return (void *) b->data + (b->written << 9);
}

static inline bool __btree_addr_written(struct btree *b, void *p)
{
	return p < write_block(b);
}

static inline bool bset_written(struct btree *b, struct bset *i)
{
	return __btree_addr_written(b, i);
}

static inline bool bkey_written(struct btree *b, struct bkey_packed *k)
{
	return __btree_addr_written(b, k);
}

static inline ssize_t __bch2_btree_u64s_remaining(struct btree *b, void *end)
{
	ssize_t used = bset_byte_offset(b, end) / sizeof(u64) +
		b->whiteout_u64s;
	ssize_t total = btree_buf_bytes(b) >> 3;

	/* Always leave one extra u64, slack for varint::decode_fast(): */
	used++;

	return total - used;
}

static inline size_t bch2_btree_keys_u64s_remaining(struct btree *b)
{
	ssize_t remaining = __bch2_btree_u64s_remaining(b,
				btree_bkey_last(b, bset_tree_last(b)));

	BUG_ON(remaining < 0);

	if (bset_written(b, btree_bset_last(b)))
		return 0;

	return remaining;
}

#define BTREE_WRITE_SET_U64s_BITS	9

static inline unsigned btree_write_set_buffer(struct btree *b)
{
	/*
	 * Could buffer up larger amounts of keys for btrees with larger keys,
	 * pending benchmarking:
	 */
	return 8 << BTREE_WRITE_SET_U64s_BITS;
}

static inline struct btree_node_entry *want_new_bset(struct bch_fs *c, struct btree *b)
{
	struct bset_tree *t = bset_tree_last(b);
	struct btree_node_entry *bne = max(write_block(b),
			(void *) btree_bkey_last(b, t));
	ssize_t remaining_space =
		__bch2_btree_u64s_remaining(b, bne->keys.start);

	if (unlikely(bset_written(b, bset(b, t)))) {
		if (b->written + block_sectors(c) <= btree_sectors(c))
			return bne;
	} else {
		if (unlikely(bset_u64s(t) * sizeof(u64) > btree_write_set_buffer(b)) &&
		    remaining_space > (ssize_t) (btree_write_set_buffer(b) >> 3))
			return bne;
	}

	return NULL;
}

/*
 * write lock must be held on @b (else the dirty bset that we were going to
 * insert into could be written out from under us)
 */
static inline bool bch2_btree_node_insert_fits(struct btree *b, unsigned u64s)
{
	if (unlikely(btree_node_need_rewrite(b)))
		return false;

	return u64s <= bch2_btree_keys_u64s_remaining(b);
}

static inline bool btree_bkey_and_val_eq(struct bkey_s_c l, struct bkey_s_c r)
{
	if (!bkey_fields_eq(l.k, r.k))
		return false;

	/* Skip mem_ptr field */
	unsigned offset = l.k->type == KEY_TYPE_btree_ptr_v2
		  ? offsetof(struct bch_btree_ptr_v2, seq)
		  : 0;

	return !memcmp((void *) l.v + offset, (void *) r.v + offset, bkey_val_bytes(l.k) - offset);
}

void bch2_btree_updates_to_text(struct printbuf *, struct bch_fs *);

static inline bool bch2_btree_interior_updates_pending(struct bch_fs *c)
{
	guard(mutex)(&c->btree.interior_updates.lock);
	return !list_empty(&c->btree.interior_updates.list);
}

bool bch2_btree_interior_updates_flush(struct bch_fs *);

void bch2_journal_entry_to_btree_root(struct bch_fs *, struct jset_entry *);

void bch2_btree_reserve_cache_to_text(struct printbuf *, struct bch_fs *);

void bch2_fs_btree_interior_update_init_early(struct bch_fs *);

#endif /* _BCACHEFS_BTREE_INTERIOR_H */
