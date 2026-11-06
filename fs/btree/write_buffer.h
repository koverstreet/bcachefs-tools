/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_BTREE_WRITE_BUFFER_H
#define _BCACHEFS_BTREE_WRITE_BUFFER_H

#include "btree/bkey.h"
#include "btree/bkey_buf.h"
#include "alloc/accounting.h"

static inline enum btree_id bch_wb_btree_to_btree_id(enum bch_wb_btree idx)
{
	static const u8 tbl[BCH_WB_BTREE_NR] = {
#define x(name) [BCH_WB_BTREE_##name] = BTREE_ID_##name,
		BCH_WRITE_BUFFER_BTREES()
#undef x
	};
	return tbl[idx];
}

#include "btree/write_buffer_defs_gen.h"

bool bch2_btree_write_buffer_flush_going_ro(struct bch_fs *);
int bch2_btree_write_buffer_tryflush(struct btree_trans *);

static inline void wb_maybe_flush_exit(struct wb_maybe_flush *f)
{
	bch2_bkey_buf_exit(&f->last_flushed);
}

static inline void wb_maybe_flush_init(struct wb_maybe_flush *f)
{
	memset(f, 0, sizeof(*f));
	bch2_bkey_buf_init(&f->last_flushed);
}

static inline int wb_maybe_flush_inc(struct wb_maybe_flush *f)
{
	f->nr_done++;
	return 0;
}

int bch2_btree_write_buffer_maybe_flush(struct btree_trans *, struct bkey_s_c, struct wb_maybe_flush *);

static inline int wb_key_cmp(const void *_l, const void *_r)
{
	const struct btree_write_buffered_key *l = _l;
	const struct btree_write_buffered_key *r = _r;

	return bpos_cmp(l->k.k.p, r->k.k.p);
}

int bch2_accounting_key_to_wb_slowpath(struct bch_fs *, enum btree_id, struct bkey_i *);

int bch2_journal_key_to_wb_slowpath(struct bch_fs *,
			     struct journal_keys_to_wb *,
			     enum bch_wb_btree, struct bkey_i *);

static inline unsigned wb_key_u64s(const struct bkey_i *k)
{
	return k->k.u64s + offsetof(struct btree_write_buffered_key, k) / sizeof(u64);
}

static inline struct btree_write_buffered_key *wb_keys_start(const struct btree_write_buffer_keys *keys)
{
	return (struct btree_write_buffered_key *) &darray_first(keys->keys);
}

static inline struct btree_write_buffered_key *wb_keys_end(const struct btree_write_buffer_keys *keys)
{
	return (struct btree_write_buffered_key *) &darray_top(keys->keys);
}

static inline struct btree_write_buffered_key *wb_keys_idx(const struct btree_write_buffer_keys *keys,
							   unsigned idx)
{
	return (struct btree_write_buffered_key *) &keys->keys.data[idx];
}

static inline struct btree_write_buffered_key *wb_key_next(const struct btree_write_buffered_key *k)
{
	return (struct btree_write_buffered_key *) ((u64 *) k + wb_key_u64s(&k->k));
}

#define wb_keys_for_each(_keys, _k)							\
	for (struct btree_write_buffered_key *_k = wb_keys_start(_keys);		\
	     _k != wb_keys_end(_keys);							\
	     _k = wb_key_next(_k))

#define wb_keys_for_each_safe(_keys, _k)						\
	for (struct btree_write_buffered_key *_next, *_k = wb_keys_start(_keys);	\
	     _k != wb_keys_end(_keys) && (_next = wb_key_next(_k), true);		\
	     _k = _next)

void bch2_journal_keys_to_write_buffer_start(struct bch_fs *, struct journal_keys_to_wb *, u64);
int bch2_journal_keys_to_write_buffer_end(struct bch_fs *, struct journal_keys_to_wb *);

int bch2_btree_write_buffer_resize(struct bch_fs *, size_t);

void bch2_btree_write_buffer_to_text(struct printbuf *, struct bch_fs *);

void bch2_btree_write_buffer_stop(struct bch_fs *);
int bch2_btree_write_buffer_start(struct bch_fs *);

void bch2_fs_btree_write_buffer_exit(struct bch_fs *);
void bch2_fs_btree_write_buffer_init_early(struct bch_fs *);
int bch2_fs_btree_write_buffer_init(struct bch_fs *);

#endif /* _BCACHEFS_BTREE_WRITE_BUFFER_H */
