/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_SNAPSHOT_H
#define _BCACHEFS_SNAPSHOT_H
#include "snapshots/snapshot_gen.h"
#include "snapshots/check_snapshots_gen.h"
#include "snapshots/delete_gen.h"

#define bch2_bkey_ops_snapshot_tree ((struct bkey_ops) {	\
	.key_validate	= bch2_snapshot_tree_validate,		\
	.val_to_text	= bch2_snapshot_tree_to_text,		\
})

#define bch2_bkey_ops_snapshot ((struct bkey_ops) {		\
	.key_validate	= bch2_snapshot_validate,		\
	.val_to_text	= bch2_snapshot_key_to_text,		\
	.trigger	= bch2_snapshot_trigger,		\
})

/*
 * The raw state field. A key that predates it reads 0, which matches no state:
 * == SNAPSHOT_STATE_deleted is false for a deleted legacy node, and
 * != SNAPSHOT_STATE_live true for a live one.
 *
 * Use bch2_snapshot_state_compat() to decide what to do with a node; this is for
 * validation and the migration, which care what's in the field.
 */
static inline enum bch_snapshot_state bch2_snapshot_state(const struct bch_snapshot *s)
{
	return le32_to_cpu(s->state);
}

static inline enum bch_snapshot_state bch2_snapshot_state_from_flags(const struct bch_snapshot *s)
{
	if (BCH_SNAPSHOT_DELETED_OBSOLETE(s))
		return SNAPSHOT_STATE_deleted;
	if (BCH_SNAPSHOT_NO_KEYS_OBSOLETE(s))
		return SNAPSHOT_STATE_no_keys;
	if (BCH_SNAPSHOT_WILL_DELETE_OBSOLETE(s))
		return SNAPSHOT_STATE_will_delete;
	return SNAPSHOT_STATE_live;
}

/*
 * The node's state, on any key: zero isn't a valid state, so a zero field means
 * the key predates it, and the old flag bits are authoritative.
 */
static inline enum bch_snapshot_state bch2_snapshot_state_compat(const struct bch_snapshot *s)
{
	return s->state
		? bch2_snapshot_state(s)
		: bch2_snapshot_state_from_flags(s);
}

static inline struct snapshot_t *__snapshot_t(struct snapshot_table *t, u32 id)
{
	u32 idx = U32_MAX - id;

	return likely(t && idx < t->nr)
		? &t->s[idx]
		: NULL;
}

static inline const struct snapshot_t *snapshot_t(struct bch_fs *c, u32 id)
{
	return __snapshot_t(rcu_dereference(c->snapshots.table), id);
}

static inline u32 bch2_snapshot_tree(struct bch_fs *c, u32 id)
{
	guard(rcu)();
	const struct snapshot_t *s = snapshot_t(c, id);
	return s ? s->tree : 0;
}

static inline u32 __bch2_snapshot_parent(struct snapshot_table *t, u32 id)
{
	const struct snapshot_t *s = __snapshot_t(t, id);
	return s ? s->parent : 0;
}

static inline u32 bch2_snapshot_parent(struct bch_fs *c, u32 id)
{
	guard(rcu)();
	return __bch2_snapshot_parent(rcu_dereference(c->snapshots.table), id);
}

static inline enum snapshot_id_state __bch2_snapshot_id_state(struct snapshot_table *t, u32 id)
{
	const struct snapshot_t *s = __snapshot_t(t, id);
	return s ? s->state : SNAPSHOT_ID_empty;
}

static inline enum snapshot_id_state bch2_snapshot_id_state(struct bch_fs *c, u32 id)
{
	guard(rcu)();
	return __bch2_snapshot_id_state(rcu_dereference(c->snapshots.table), id);
}

static inline bool bch2_snapshot_exists(struct bch_fs *c, u32 id)
{
	return bch2_snapshot_id_state(c, id) == SNAPSHOT_ID_live;
}

static inline int bch2_snapshot_is_internal_node(struct bch_fs *c, u32 id)
{
	guard(rcu)();
	const struct snapshot_t *s = snapshot_t(c, id);
	return s ? s->children[0] != 0 : bch_err_throw(c, invalid_snapshot_node);
}

static inline int bch2_snapshot_is_leaf(struct bch_fs *c, u32 id)
{
	int ret = bch2_snapshot_is_internal_node(c, id);
	if (ret < 0)
		return ret;
	return !ret;
}

static inline bool bch2_snapshot_is_ancestor(struct btree_trans *trans, u32 id, u32 ancestor)
{
	EBUG_ON(!id);
	EBUG_ON(!ancestor);

	return id == ancestor
		? true
		: __bch2_snapshot_is_ancestor(trans, id, ancestor);
}

static inline bool bch2_snapshot_has_children(struct bch_fs *c, u32 id)
{
	guard(rcu)();
	const struct snapshot_t *t = snapshot_t(c, id);
	return t && (t->children[0]|t->children[1]) != 0;
}

static inline bool snapshot_list_has_id(snapshot_id_list *s, u32 id)
{
	return darray_find(*s, id) != NULL;
}

static inline bool snapshot_list_has_ancestor(struct btree_trans *trans, snapshot_id_list *s, u32 id)
{
	return darray_find_p(*s, i, bch2_snapshot_is_ancestor(trans, id, *i)) != NULL;
}

static inline int snapshot_list_add(struct bch_fs *c, snapshot_id_list *s, u32 id)
{
	BUG_ON(snapshot_list_has_id(s, id));
	int ret = darray_push(s, id);
	if (ret)
		bch_err(c, "error reallocating snapshot_id_list (size %zu)", s->size);
	return ret;
}

static inline int bch2_check_key_has_snapshot(struct btree_trans *trans,
					      struct btree_iter *iter,
					      struct bkey_s_c k)
{
	return likely(bch2_snapshot_exists(trans->c, k.k->p.snapshot))
		? 0
		: __bch2_check_key_has_snapshot(trans, iter, k);
}

int __bch2_get_snapshot_overwrites(struct btree_trans *,
				   enum btree_id, struct bpos,
				   snapshot_id_list *);

/*
 * Get a list of snapshot IDs that have overwritten a given key:
 */
static inline int bch2_get_snapshot_overwrites(struct btree_trans *trans,
					       enum btree_id btree, struct bpos pos,
					       snapshot_id_list *s)
{
	return bch2_snapshot_has_children(trans->c, pos.snapshot)
		? __bch2_get_snapshot_overwrites(trans, btree, pos, s)
		: 0;

}

int __bch2_key_has_snapshot_overwrites(struct btree_trans *, enum btree_id, struct bpos);

static inline int bch2_key_has_snapshot_overwrites(struct btree_trans *trans,
					  enum btree_id id,
					  struct bpos pos)
{
	if (!btree_type_has_snapshots(id) ||
	    bch2_snapshot_is_leaf(trans->c, pos.snapshot) > 0)
		return 0;

	return __bch2_key_has_snapshot_overwrites(trans, id, pos);
}

#endif /* _BCACHEFS_SNAPSHOT_H */
