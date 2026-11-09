/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_SNAPSHOT_H
#define _BCACHEFS_SNAPSHOT_H

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

static inline int bch2_snapshot_is_internal_node(struct bch_fs *c, u32 id)
{
	guard(rcu)();
	const struct snapshot_t *s = snapshot_t(c, id);
	return s ? s->children[0] != 0 : bch_err_throw(c, invalid_snapshot_node);
}

#endif /* _BCACHEFS_SNAPSHOT_H */
