/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_BTREE_TYPES_INLINE_H
#define _BCACHEFS_BTREE_TYPES_INLINE_H

/* Included at the end of btree/types.h. */

static inline size_t btree_cache_list_nr(const struct btree_cache_list *l)
{
	return l->nr_clean + l->nr_dirty;
}

static inline size_t btree_cache_nr_live(const struct bch_fs_btree_cache *bc)
{
	return btree_cache_list_nr(&bc->live[0]) +
		btree_cache_list_nr(&bc->live[1]);
}

static inline size_t btree_cache_nr_dirty(const struct bch_fs_btree_cache *bc)
{
	return bc->live[0].nr_dirty + bc->live[1].nr_dirty;
}

/* iter flags must fit in struct btree_iter.flags: */
static_assert(BTREE_ITER_FLAG_BIT_committed < 32);

static inline struct btree_path_level *path_l(struct btree_path *path)
{
	return path->l + path->level;
}

static inline unsigned long btree_path_ip_allocated(struct btree_path *path)
{
#ifdef TRACK_PATH_ALLOCATED
	return path->ip_allocated;
#else
	return _THIS_IP_;
#endif
}

static inline struct bpos btree_node_pos(struct btree_bkey_cached_common *b)
{
	return !b->cached
		? container_of(b, struct btree, c)->key.k.p
		: container_of(b, struct bkey_cached, c)->key.pos;
}

static inline struct btree_path *btree_iter_path(struct btree_trans *trans, struct btree_iter *iter)
{
	return trans->paths + iter->path;
}

static inline struct btree_path *btree_iter_key_cache_path(struct btree_trans *trans, struct btree_iter *iter)
{
	return iter->key_cache_path
		? trans->paths + iter->key_cache_path
		: NULL;
}

#define x(flag)								\
static inline bool btree_node_ ## flag(const struct btree *b)		\
{	return test_bit(BTREE_NODE_ ## flag, &b->flags); }		\
									\
static inline void set_btree_node_ ## flag(struct btree *b)		\
{	set_bit(BTREE_NODE_ ## flag, &b->flags); }			\
									\
static inline void clear_btree_node_ ## flag(struct btree *b)		\
{	clear_bit(BTREE_NODE_ ## flag, &b->flags); }

BTREE_FLAGS()
#undef x

/* Btree node/bset accessors: */

static inline enum btree_node_rewrite_reason btree_node_rewrite_reason(struct btree *b)
{
	if (btree_node_need_rewrite_ptr_written_zero(b))
		return BTREE_NODE_REWRITE_ptr_written_zero;
	if (btree_node_need_rewrite_error(b))
		return BTREE_NODE_REWRITE_error;
	if (btree_node_need_rewrite(b))
		return BTREE_NODE_REWRITE_unknown;
	return BTREE_NODE_REWRITE_none;
}

static inline struct btree_write *btree_current_write(struct btree *b)
{
	return b->writes + btree_node_write_idx(b);
}

static inline struct btree_write *btree_prev_write(struct btree *b)
{
	return b->writes + (btree_node_write_idx(b) ^ 1);
}

static inline struct bset_tree *bset_tree_last(struct btree *b)
{
	EBUG_ON(!b->nsets);
	return b->set + b->nsets - 1;
}

static inline void *
__btree_node_offset_to_ptr(const struct btree *b, u16 offset)
{
	return (void *) ((u64 *) b->data + offset);
}

static inline u16
__btree_node_ptr_to_offset(const struct btree *b, const void *p)
{
	u16 ret = (u64 *) p - (u64 *) b->data;

	EBUG_ON(__btree_node_offset_to_ptr(b, ret) != p);
	return ret;
}

static inline struct bset *bset(const struct btree *b,
				const struct bset_tree *t)
{
	return __btree_node_offset_to_ptr(b, t->data_offset);
}

static inline void set_btree_bset_end(struct btree *b, struct bset_tree *t)
{
	t->end_offset =
		__btree_node_ptr_to_offset(b, vstruct_last(bset(b, t)));
}

static inline void set_btree_bset(struct btree *b, struct bset_tree *t,
				  const struct bset *i)
{
	t->data_offset = __btree_node_ptr_to_offset(b, i);
	set_btree_bset_end(b, t);
}

static inline struct bset *btree_bset_first(struct btree *b)
{
	return bset(b, b->set);
}

static inline struct bset *btree_bset_last(struct btree *b)
{
	return bset(b, bset_tree_last(b));
}

static inline u16
__btree_node_key_to_offset(const struct btree *b, const struct bkey_packed *k)
{
	return __btree_node_ptr_to_offset(b, k);
}

static inline struct bkey_packed *
__btree_node_offset_to_key(const struct btree *b, u16 k)
{
	return __btree_node_offset_to_ptr(b, k);
}

static inline unsigned btree_bkey_first_offset(const struct bset_tree *t)
{
	return t->data_offset + offsetof(struct bset, _data) / sizeof(u64);
}

static inline unsigned bset_u64s(struct bset_tree *t)
{
	return t->end_offset - t->data_offset -
		sizeof(struct bset) / sizeof(u64);
}

static inline unsigned bset_dead_u64s(struct btree *b, struct bset_tree *t)
{
	return bset_u64s(t) - b->nr.bset_u64s[t - b->set];
}

static inline unsigned bset_byte_offset(struct btree *b, void *i)
{
	return i - (void *) b->data;
}

/* Type of a key in btree @id at level @level: */
static inline enum btree_node_type __btree_node_type(unsigned level, enum btree_id id)
{
	return level ? BKEY_TYPE_btree : (unsigned) id + 1;
}

/* Type of keys @b contains: */
static inline enum btree_node_type btree_node_type(struct btree *b)
{
	return __btree_node_type(b->c.level, b->c.btree_id);
}

const char *bch2_btree_node_type_str(enum btree_node_type);

/*
 * Mask of btree ids that have snapshots; defined here, ahead of the other
 * btree-id masks below, because the trigger masks build on it: every snapshot
 * btree needs trans triggers so that bch2_trigger_snapshot_nr_keys() runs for
 * its keys. (<< 1 maps btree_id space to btree_node_type space, where a leaf's
 * node type is id + 1.)
 */
static const u64 btree_has_snapshots_mask = 0
#define x(name, nr, flags, ...)	|((!!((flags) & BTREE_IS_snapshots)) << nr)
BCH_BTREE_IDS()
#undef x
;

#define BTREE_NODE_TYPE_HAS_TRANS_TRIGGERS		\
	(BIT_ULL(BKEY_TYPE_extents)|			\
	 BIT_ULL(BKEY_TYPE_alloc)|			\
	 BIT_ULL(BKEY_TYPE_inodes)|			\
	 BIT_ULL(BKEY_TYPE_stripes)|			\
	 BIT_ULL(BKEY_TYPE_reflink)|			\
	 BIT_ULL(BKEY_TYPE_subvolumes)|			\
	 BIT_ULL(BKEY_TYPE_snapshots)|			\
	 BIT_ULL(BKEY_TYPE_btree)|			\
	 (btree_has_snapshots_mask << 1))

#define BTREE_NODE_TYPE_HAS_ATOMIC_TRIGGERS		\
	(BIT_ULL(BKEY_TYPE_alloc)|			\
	 BIT_ULL(BKEY_TYPE_inodes)|			\
	 BIT_ULL(BKEY_TYPE_stripes)|			\
	 BIT_ULL(BKEY_TYPE_snapshots))

#define BTREE_NODE_TYPE_HAS_TRIGGERS			\
	(BTREE_NODE_TYPE_HAS_TRANS_TRIGGERS|		\
	 BTREE_NODE_TYPE_HAS_ATOMIC_TRIGGERS)

static inline bool btree_node_type_has_trans_triggers(enum btree_node_type type)
{
	return BIT_ULL(type) & BTREE_NODE_TYPE_HAS_TRANS_TRIGGERS;
}

static inline bool btree_node_type_has_atomic_triggers(enum btree_node_type type)
{
	return BIT_ULL(type) & BTREE_NODE_TYPE_HAS_ATOMIC_TRIGGERS;
}

static inline bool btree_node_type_has_triggers(enum btree_node_type type)
{
	return BIT_ULL(type) & BTREE_NODE_TYPE_HAS_TRIGGERS;
}

/* A mask of btree id bits that have triggers for their leaves */
__maybe_unused
static const u64 btree_leaf_has_triggers_mask = BTREE_NODE_TYPE_HAS_TRIGGERS >> 1;

static const u64 btree_is_extents_mask = 0
#define x(name, nr, flags, ...)	|((!!((flags) & BTREE_IS_extents)) << nr)
BCH_BTREE_IDS()
#undef x
;

static inline bool btree_id_is_extents(enum btree_id btree)
{
	return BIT_ULL(btree) & btree_is_extents_mask;
}

static inline bool btree_node_type_is_extents(enum btree_node_type type)
{
	return type != BKEY_TYPE_btree && btree_id_is_extents(type - 1);
}

static inline bool btree_type_has_snapshots(enum btree_id btree)
{
	return BIT_ULL(btree) & btree_has_snapshots_mask;
}

static inline bool btree_id_is_extents_snapshots(enum btree_id btree)
{
	return BIT_ULL(btree) & btree_has_snapshots_mask & btree_is_extents_mask;
}

static inline bool btree_type_has_snapshot_field(enum btree_id btree)
{
	const u64 mask = 0
#define x(name, nr, flags, ...)	|((!!((flags) & (BTREE_IS_snapshot_field|BTREE_IS_snapshots))) << nr)
	BCH_BTREE_IDS()
#undef x
	;

	return BIT_ULL(btree) & mask;
}

static const u64 btree_has_data_ptrs_mask = 0
#define x(name, nr, flags, ...)	|((!!((flags) & BTREE_IS_data)) << nr)
	BCH_BTREE_IDS()
#undef x
	;

static inline bool btree_type_has_data_ptrs(enum btree_id btree)
{
	return BIT_ULL(btree) & btree_has_data_ptrs_mask;
}

static inline bool btree_type_uses_write_buffer(enum btree_id btree)
{
	const u64 mask = 0
#define x(name, nr, flags, ...)	|((!!((flags) & BTREE_IS_write_buffer)) << nr)
	BCH_BTREE_IDS()
#undef x
	;

	return BIT_ULL(btree) & mask;
}

static inline u8 btree_trigger_order(enum btree_id btree)
{
	switch (btree) {
	case BTREE_ID_alloc:
		return U8_MAX;
	case BTREE_ID_stripes:
		return U8_MAX - 1;
	default:
		return btree;
	}
}

#endif /* _BCACHEFS_BTREE_TYPES_INLINE_H */
