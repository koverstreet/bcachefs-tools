// SPDX-License-Identifier: GPL-2.0
#include "bcachefs.h"

#include "alloc/accounting.h"

#include "btree/cache.h"
#include "btree/update.h"

#include "snapshots/snapshot.h"
#include "snapshots/subvolume.h"

#include "init/error.h"
#include "init/passes.h"
#include "init/progress.h"

static int bch2_snapshot_table_make_room(struct bch_fs *c, u32 id)
{
	guard(mutex)(&c->snapshots.table_lock);
	return bch2_snapshot_t_mut(c, id)
		? 0
		: bch_err_throw(c, ENOMEM_mark_snapshot);
}

static int bch2_snapshot_tree_create(struct btree_trans *trans,
				u32 root_id, u32 subvol_id, u32 *tree_id)
{
	struct bkey_i_snapshot_tree *n_tree =
		__bch2_snapshot_tree_create(trans);

	if (IS_ERR(n_tree))
		return PTR_ERR(n_tree);

	n_tree->v.master_subvol	= cpu_to_le32(subvol_id);
	n_tree->v.root_snapshot	= cpu_to_le32(root_id);
	*tree_id = n_tree->k.p.offset;
	return 0;
}

static u32 bch2_snapshot_oldest_subvol(struct bch_fs *c, u32 snapshot_root,
				       snapshot_id_list *skip)
{
	guard(rcu)();
	struct snapshot_table *t = rcu_dereference(c->snapshots.table);

	while (true) {
		u32 subvol = 0;

		__for_each_snapshot_child(c, t, snapshot_root, NULL, id)  {
			if (skip && snapshot_list_has_id(skip, id))
				continue;

			u32 s = __snapshot_t(t, id)->subvol;
			if (s && (!subvol || s < subvol))
				subvol = s;
		}

		if (subvol || !skip)
			return subvol;

		skip = NULL;
	}
}

static int bch2_snapshot_tree_master_subvol(struct btree_trans *trans,
					    u32 snapshot_root, u32 *subvol_id)
{
	struct bch_fs *c = trans->c;
	struct bkey_s_c k;
	int ret;

	for_each_btree_key_norestart(trans, iter, BTREE_ID_subvolumes, POS_MIN,
				     0, k, ret) {
		if (k.k->type != KEY_TYPE_subvolume)
			continue;

		struct bkey_s_c_subvolume s = bkey_s_c_to_subvolume(k);
		if (!bch2_snapshot_is_ancestor(trans, le32_to_cpu(s.v->snapshot), snapshot_root))
			continue;
		if (!BCH_SUBVOLUME_SNAP(s.v)) {
			*subvol_id = s.k->p.offset;
			return 0;
		}
	}
	if (ret)
		return ret;

	*subvol_id = bch2_snapshot_oldest_subvol(c, snapshot_root, NULL);

	struct bkey_i_subvolume *u =
		errptr_try(bch2_bkey_get_mut_typed(trans, BTREE_ID_subvolumes, POS(0, *subvol_id),
					0, subvolume));

	SET_BCH_SUBVOLUME_SNAP(&u->v, false);
	return 0;
}

static int check_snapshot_tree(struct btree_trans *trans,
			       struct btree_iter *iter,
			       struct bkey_s_c k)
{
	struct bch_fs *c = trans->c;
	CLASS(printbuf, buf)();

	if (k.k->type != KEY_TYPE_snapshot_tree)
		return 0;

	struct bkey_s_c_snapshot_tree st = bkey_s_c_to_snapshot_tree(k);
	u32 root_id = le32_to_cpu(st.v->root_snapshot);

	CLASS(btree_iter, snapshot_iter)(trans, BTREE_ID_snapshots, POS(0, root_id), 0);
	struct bkey_s_c_snapshot snapshot_k = bch2_bkey_get_typed(&snapshot_iter, snapshot);
	int ret = bkey_err(snapshot_k);
	if (ret && !bch2_err_matches(ret, ENOENT))
		return ret;

	struct bch_snapshot s;
	if (!ret)
		bkey_val_copy_pad(&s, snapshot_k);

	if (fsck_err_on(ret ||
			root_id != bch2_snapshot_root(c, root_id) ||
			st.k->p.offset != le32_to_cpu(s.tree),
			trans, snapshot_tree_to_missing_snapshot,
			"snapshot tree points to missing/incorrect snapshot:\n%s",
			(bch2_bkey_val_to_text(&buf, c, st.s_c),
			 prt_newline(&buf),
			 ret
			 ? prt_printf(&buf, "(%s)", bch2_err_str(ret))
			 : bch2_bkey_val_to_text(&buf, c, snapshot_k.s_c),
			 buf.buf)))
		return bch2_btree_delete_at(trans, iter, 0);

	if (!st.v->master_subvol)
		return 0;

	struct bch_subvolume subvol;
	ret = bch2_subvolume_get(trans, le32_to_cpu(st.v->master_subvol), false, &subvol);
	if (ret && !bch2_err_matches(ret, ENOENT))
		return ret;

	if (fsck_err_on(ret,
			trans, snapshot_tree_to_missing_subvol,
			"snapshot tree points to missing subvolume:\n%s",
			(printbuf_reset(&buf),
			 bch2_bkey_val_to_text(&buf, c, st.s_c), buf.buf)) ||
	    fsck_err_on(!ret &&
			!bch2_snapshot_is_ancestor(trans,
						le32_to_cpu(subvol.snapshot),
						root_id),
			trans, snapshot_tree_to_wrong_subvol,
			"snapshot tree points to subvolume that does not point to snapshot in this tree:\n%s",
			(printbuf_reset(&buf),
			 bch2_bkey_val_to_text(&buf, c, st.s_c), buf.buf)) ||
	    fsck_err_on(!ret && BCH_SUBVOLUME_SNAP(&subvol),
			trans, snapshot_tree_to_snapshot_subvol,
			"snapshot tree points to snapshot subvolume:\n%s",
			(printbuf_reset(&buf),
			 bch2_bkey_val_to_text(&buf, c, st.s_c), buf.buf))) {
		u32 subvol_id;
		ret = bch2_snapshot_tree_master_subvol(trans, root_id, &subvol_id);
		bch_err_fn(c, ret);

		if (bch2_err_matches(ret, ENOENT)) /* nothing to be done here */
			return 0;

		if (ret)
			return ret;

		struct bkey_i_snapshot_tree *u =
			errptr_try(bch2_bkey_make_mut_typed(trans, iter, &k, 0, snapshot_tree));

		u->v.master_subvol = cpu_to_le32(subvol_id);
		st = snapshot_tree_i_to_s_c(u);
	}
fsck_err:
	return ret;
}

/*
 * For each snapshot_tree, make sure it points to the root of a snapshot tree
 * and that snapshot entry points back to it, or delete it.
 *
 * And, make sure it points to a subvolume within that snapshot tree, or correct
 * it to point to the oldest subvolume within that snapshot tree.
 */
int bch2_check_snapshot_trees(struct bch_fs *c)
{
	CLASS(btree_trans, trans)(c);
	return for_each_btree_key_commit(trans, iter,
			BTREE_ID_snapshot_trees, POS_MIN,
			BTREE_ITER_prefetch, k,
			NULL, NULL, BCH_TRANS_COMMIT_no_enospc,
		check_snapshot_tree(trans, &iter, k));
}

/*
 * Look up snapshot tree for @tree_id and find root,
 * make sure @snap_id is a descendent:
 */
static int snapshot_tree_ptr_good(struct btree_trans *trans,
				  u32 snap_id, u32 tree_id)
{
	struct bch_snapshot_tree s_t;
	int ret = bch2_snapshot_tree_lookup(trans, tree_id, &s_t);

	if (bch2_err_matches(ret, ENOENT))
		return 0;
	if (ret)
		return ret;

	return bch2_snapshot_is_ancestor_early(trans->c, snap_id, le32_to_cpu(s_t.root_snapshot));
}

u32 bch2_snapshot_skiplist_get(struct bch_fs *c, u32 id)
{
	if (!id)
		return 0;

	guard(rcu)();
	const struct snapshot_t *s = snapshot_t(c, id);
	return s->parent
		? bch2_snapshot_nth_parent(c, id, get_random_u32_below(s->depth))
		: id;
}

/*
 * snapshot_tree pointer was incorrect: look up root snapshot node, make sure
 * its snapshot_tree pointer is correct (allocate new one if necessary), then
 * update this node's pointer to root node's pointer:
 */
static int snapshot_tree_ptr_repair(struct btree_trans *trans,
				    struct btree_iter *iter,
				    struct bkey_s_c k,
				    struct bch_snapshot *s)
{
	struct bch_fs *c = trans->c;
	u32 root_id = bch2_snapshot_root(c, k.k->p.offset);

	CLASS(btree_iter, root_iter)(trans, BTREE_ID_snapshots, POS(0, root_id), 0);
	struct bkey_s_c_snapshot root = bkey_try(bch2_bkey_get_typed(&root_iter, snapshot));

	u32 tree_id = le32_to_cpu(root.v->tree);

	struct bch_snapshot_tree s_t;
	int ret = bch2_snapshot_tree_lookup(trans, tree_id, &s_t);
	if (ret && !bch2_err_matches(ret, ENOENT))
		return ret;

	if (ret || le32_to_cpu(s_t.root_snapshot) != root_id) {
		struct bkey_i_snapshot *u =
			errptr_try(bch2_bkey_make_mut_typed(trans, &root_iter, &root.s_c, 0, snapshot));

		try(bch2_snapshot_tree_create(trans, root_id,
					      bch2_snapshot_oldest_subvol(c, root_id, NULL),
					      &tree_id));

		u->v.tree = cpu_to_le32(tree_id);
		if (k.k->p.offset == root_id)
			*s = u->v;
	}

	if (k.k->p.offset != root_id) {
		struct bkey_i_snapshot *u =
			errptr_try(bch2_bkey_make_mut_typed(trans, iter, &k, 0, snapshot));

		u->v.tree = cpu_to_le32(tree_id);
		*s = u->v;
	}

	return 0;
}

static int check_snapshot_to_subvol(struct btree_trans *trans,
			  struct btree_iter *iter,
			  struct bkey_s_c k,
			  struct bch_snapshot *s,
			  struct bkey_i_snapshot *u)
{
	struct bch_fs *c = trans->c;
	CLASS(printbuf, buf)();

	bool should_have_subvol = !s->children[0] &&
		(bch2_snapshot_state(s) == SNAPSHOT_STATE_live ||
		 bch2_snapshot_state(s) == SNAPSHOT_STATE_will_delete);

	if (should_have_subvol && s->subvol) {
		/* dangling snapshot will be handled later */
		u32 id = le32_to_cpu(s->subvol);

		struct bch_subvolume subvol;
		int ret = bch2_subvolume_get(trans, id, false, &subvol);
		if (bch2_err_matches(ret, ENOENT))
			bch_err(c, "snapshot points to nonexistent subvolume:\n  %s",
				(bch2_bkey_val_to_text(&buf, c, k), buf.buf));
		if (ret)
			return ret;
	} else {
		if (ret_fsck_err_on(s->subvol,
				trans, snapshot_should_not_have_subvol,
				"snapshot should not point to subvol:\n%s",
				(bch2_bkey_val_to_text(&buf, c, k), buf.buf))) {
			u = u ?: errptr_try(bch2_bkey_make_mut_typed(trans, iter, &k, 0, snapshot));

			/* XXX: DANGEROUS */

			u->v.subvol = 0;
			*s = u->v;
		}
	}

	if (ret_fsck_err_on(BCH_SNAPSHOT_SUBVOL_OBSOLETE(s) != (s->subvol != 0),
			    trans, snapshot_subvol_flag_wrong,
			    "snapshot node %llu has wrong subvol flag",
			    k.k->p.offset)) {
		u = u ?: errptr_try(bch2_bkey_make_mut_typed(trans, iter, &k, 0, snapshot));

		SET_BCH_SNAPSHOT_SUBVOL_OBSOLETE(&u->v, s->subvol != 0);
		*s = u->v;
	}

	return 0;
}

/*
 * Parent <-> child edge checks and repair.
 *
 * An edge is repaired only when doubly attested: the surviving pointer plus
 * corroboration (id ordering, tree, depth - checked before the depth/skip
 * autofixes rewrite them). Ambiguous evidence fail-stops; a wrong join or
 * split moves keys between subvolume visibilities. Never write "I don't
 * know" to disk: a node with a zeroed parent masquerades as a tree root and
 * gets consumed by the tree-pointer repair or the deletion machinery.
 * Repairs commit and restart, so decisions only see settled state. The
 * in-memory snapshot table serves as the reverse index (live nodes only:
 * a tombstone's child pointer is a splice breadcrumb, not a claim - I1).
 */

enum { EDGE_PARENT, EDGE_CHILD };

static bool snapshot_node_points_back(const struct bch_snapshot *s, unsigned side, u32 other)
{
	return side == EDGE_PARENT
		? le32_to_cpu(s->children[0]) == other ||
		  le32_to_cpu(s->children[1]) == other
		: le32_to_cpu(s->parent) == other;
}

/*
 * Find the unique live node in role @side claiming an edge with @id,
 * excluding everything @s already references (intact edges, stale entries
 * for the disputed pointer, cycles):
 */
static u32 snapshot_table_find_edge(struct bch_fs *c, const struct bch_snapshot *s,
				    u32 id, unsigned side)
{
	guard(rcu)();
	struct snapshot_table *t = rcu_dereference(c->snapshots.table);
	u32 found = 0;

	for (size_t idx = 0; idx < t->nr; idx++) {
		const struct snapshot_t *n = &t->s[idx];
		u32 n_id = U32_MAX - idx;

		if (n->state != SNAPSHOT_ID_live)
			continue;

		if (!(side == EDGE_PARENT
		      ? n->children[0] == id || n->children[1] == id
		      : n->parent == id))
			continue;

		if (n_id == le32_to_cpu(s->parent) ||
		    n_id == le32_to_cpu(s->children[0]) ||
		    n_id == le32_to_cpu(s->children[1]))
			continue;

		if (found)
			return 0;	/* ambiguous */
		found = n_id;
	}
	return found;
}

static u64 snapshot_data_sectors(struct bch_fs *c, u32 id)
{
	struct disk_accounting_pos acc;
	memset(&acc, 0, sizeof(acc));
	acc.type = BCH_DISK_ACCOUNTING_snapshot;
	acc.snapshot.id = id;

	u64 sectors = 0;
	bch2_accounting_mem_read(c, disk_accounting_pos_to_bpos(&acc), &sectors, 1);
	return sectors;
}

static bool snapshot_parent_child_consistent(const struct bch_snapshot *s, u32 id, unsigned side,
				       const struct bch_snapshot *o, u32 o_id)
{
	const struct bch_snapshot *pa = side == EDGE_PARENT ? s : o;
	const struct bch_snapshot *ch = side == EDGE_PARENT ? o : s;
	u32 pa_id = side == EDGE_PARENT ? id : o_id;
	u32 ch_id = side == EDGE_PARENT ? o_id : id;

	return ch_id < pa_id &&
	       pa->tree == ch->tree &&
	       le32_to_cpu(pa->depth) + 1 == le32_to_cpu(ch->depth);
}

/*
 * Does @n (in role @side) have a pointer position that's empty, or whose
 * current target doesn't reciprocate? Returns the displaced value in
 * @old_id; empty positions are preferred, so a refuted value keeps its shot
 * at repair via its own scan:
 */
static int snapshot_edge_ptr_available(struct btree_trans *trans,
				       const struct bch_snapshot *n, u32 n_id,
				       unsigned side, u32 *old_id)
{
	u32 ptrs[2];
	unsigned nr;

	if (side == EDGE_CHILD) {
		ptrs[0]	= le32_to_cpu(n->parent);
		nr	= 1;
	} else {
		ptrs[0]	= le32_to_cpu(n->children[0]);
		ptrs[1]	= le32_to_cpu(n->children[1]);
		nr	= 2;
	}

	for (unsigned i = 0; i < nr; i++)
		if (!ptrs[i]) {
			*old_id = 0;
			return 1;
		}

	for (unsigned i = 0; i < nr; i++) {
		struct bch_snapshot t;
		int ret = bch2_snapshot_lookup(trans, ptrs[i], &t);
		if (ret && !bch2_err_matches(ret, ENOENT))
			return ret;

		if (ret || !snapshot_node_points_back(&t, !side, n_id)) {
			*old_id = ptrs[i];
			return 1;
		}
	}

	return 0;
}

static int snapshot_edge_repair_commit(struct btree_trans *trans)
{
	try(bch2_trans_commit(trans, NULL, NULL, BCH_TRANS_COMMIT_no_enospc));
	trans->c->snapshots.need_table_rebuild = true;
	return bch_err_throw(trans->c, transaction_restart_nested);
}

/* Rewrite @node_id's reference to @old_id (0 clears a child slot): */
static int snapshot_edge_set_ptr(struct btree_trans *trans, u32 node_id,
				 unsigned side, u32 old_id, u32 new_id)
{
	struct bkey_i_snapshot *n =
		errptr_try(bch2_bkey_get_mut_typed(trans, BTREE_ID_snapshots,
						   POS(0, node_id), 0, snapshot));

	if (side == EDGE_CHILD) {
		n->v.parent = cpu_to_le32(new_id);
	} else {
		for (unsigned i = 0; i < 2; i++)
			if (le32_to_cpu(n->v.children[i]) == old_id) {
				n->v.children[i] = cpu_to_le32(new_id);
				break;
			}

		if (le32_to_cpu(n->v.children[0]) < le32_to_cpu(n->v.children[1]))
			swap(n->v.children[0], n->v.children[1]);
	}

	return snapshot_edge_repair_commit(trans);
}

static int check_snapshot_edge(struct btree_trans *trans,
			       const struct bch_snapshot *s, u32 id,
			       unsigned side, u32 other_id)
{
	struct bch_fs *c = trans->c;

	struct bch_snapshot other;
	int other_ret = bch2_snapshot_lookup(trans, other_id, &other);
	if (other_ret && !bch2_err_matches(other_ret, ENOENT))
		return other_ret;
	bool other_exists = !other_ret;

	if (other_exists && snapshot_node_points_back(&other, !side, id))
		return 0;

	/*
	 * Our claim completes the edge if the target's position toward us is
	 * empty or refuted - but never un-tombstone a node:
	 */
	if (other_exists &&
	    bch2_snapshot_state_compat(&other) != SNAPSHOT_STATE_deleted &&
	    snapshot_parent_child_consistent(s, id, side, &other, other_id)) {
		u32 old = 0;
		int avail = snapshot_edge_ptr_available(trans, &other, other_id, !side, &old);
		if (avail < 0)
			return avail;

		if (avail &&
		    ret_fsck_err(trans, snapshot_edge_bad,
				 "snapshot %u %s pointer %u is not reciprocated, but is corroborated by\n"
				 "tree and depth and the target's position (%u) is unattested - completing the edge",
				 id, side == EDGE_CHILD ? "parent" : "child", other_id, old))
			return snapshot_edge_set_ptr(trans, other_id, !side, old, id);
	}

	/* Or a corroborated claimant is the true counterpart - re-aim ours: */
	u32 repl = snapshot_table_find_edge(c, s, id, !side);

	struct bch_snapshot r;
	int repl_ret = repl ? bch2_snapshot_lookup(trans, repl, &r) : -ENOENT;
	if (repl_ret && !bch2_err_matches(repl_ret, ENOENT))
		return repl_ret;

	if (!repl_ret &&
	    snapshot_parent_child_consistent(s, id, side, &r, repl) &&
	    ret_fsck_err(trans, snapshot_edge_bad,
			 "snapshot %u %s pointer %u is broken (target %s), but node %u claims the\n"
			 "edge, corroborated by tree and depth - repairing",
			 id, side == EDGE_CHILD ? "parent" : "child", other_id,
			 other_exists ? "does not reciprocate" : "does not exist",
			 repl))
		return snapshot_edge_set_ptr(trans, id, side, other_id, repl);

	/* A dangling child pointer with no data accounted to it may be cleared: */
	if (side == EDGE_PARENT && !other_exists) {
		u32 sibling = le32_to_cpu(s->children[0]) == other_id
			? le32_to_cpu(s->children[1])
			: le32_to_cpu(s->children[0]);
		u64 sectors = snapshot_data_sectors(c, other_id);

		if (!sectors && sibling &&
		    ret_fsck_err(trans, snapshot_edge_bad,
				 "snapshot %u child pointer %u does not exist: nothing claims %u as\n"
				 "parent and no data is accounted to it - clearing",
				 id, other_id, id))
			return snapshot_edge_set_ptr(trans, id, side, other_id, 0);
	}

	/*
	 * A single stomped field always leaves the other side's intact
	 * pointer for the repairs above to key off, so reaching here means
	 * multiple corruptions or a destroyed node - beyond what local
	 * evidence can repair. Rare enough that we report and stop rather
	 * than attempt topology surgery on a conjunction of corruptions:
	 */
	{
		CLASS(printbuf, buf)();
		prt_printf(&buf, "snapshot topology damage is beyond single-corruption repair:\n"
			   "node %u's %s pointer names %u, which %s\n",
			   id, side == EDGE_CHILD ? "parent" : "child", other_id,
			   other_exists
			   ? "exists but does not point back, and tree/depth do not identify them as parent and child"
			   : "does not exist");

		prt_printf(&buf, "no other node passes the parent/child consistency checks for this edge\n");

		prt_printf(&buf, "node:   ");
		bch2_snapshot_to_text(&buf, s);
		prt_newline(&buf);

		if (other_exists) {
			prt_printf(&buf, "target: ");
			bch2_snapshot_to_text(&buf, &other);
			prt_newline(&buf);
		}

		prt_printf(&buf, "not repairing: run fsck; if damage is extensive, reconstruct_snapshots rebuilds topology from key evidence");
		bch_err(c, "%s", buf.buf);
	}

	if (!other_exists)
		return other_ret;

	return side == EDGE_CHILD
		? bch_err_throw(c, EINVAL_snapshot_parent_missing_child_ptr)
		: bch_err_throw(c, EINVAL_snapshot_child_bad_parent);
}

static int check_snapshot(struct btree_trans *trans,
			  struct btree_iter *iter,
			  struct bkey_s_c k)
{
	struct bch_fs *c = trans->c;
	CLASS(printbuf, buf)();
	struct bkey_i_snapshot *u = NULL;
	int ret = 0;

	if (k.k->type != KEY_TYPE_snapshot)
		return 0;

	struct bch_snapshot s;
	bkey_val_copy_pad(&s, bkey_s_c_to_snapshot(k));

	/*
	 * On upgrade the flag bits are authoritative - an old kernel may have
	 * written them while unable to maintain the state field:
	 */
	if (c->sb.version_upgrade_complete < bcachefs_metadata_version_per_dev_fragmentation_lru &&
	    bch2_snapshot_state(&s) != bch2_snapshot_state_from_flags(&s)) {
		u = u ?: errptr_try(bch2_bkey_make_mut_typed(trans, iter, &k, 0, snapshot));
		u->v.state = cpu_to_le32(bch2_snapshot_state_from_flags(&s));
		s = u->v;
	}

	/*
	 * Pre-upgrade, the rewrite above always leaves a valid state, so this
	 * only fires post-upgrade - where any invalid value (including zero)
	 * is corruption. No repair yet, and state-keyed repairs must not run
	 * on a state we can't read:
	 */
	if (!bch2_snapshot_state_valid(bch2_snapshot_state(&s))) {
		CLASS(bch_log_msg, msg)(c);

		prt_printf(&msg.m, "snapshot has invalid state 0x%x:\n",
			   le32_to_cpu(s.state));
		bch2_bkey_val_to_text(&msg.m, c, k);
		msg.m.suppress = !bch2_count_fsck_err(c, snapshot_state_bad, &msg.m);

		return bch_err_throw(c, fsck_repair_unimplemented);
	}

	if (bch2_snapshot_state(&s) == SNAPSHOT_STATE_deleted)
		return 0;

	if (s.parent)
		try(check_snapshot_edge(trans, &s, k.k->p.offset,
					EDGE_CHILD, le32_to_cpu(s.parent)));

	for (unsigned i = 0; i < 2; i++)
		if (s.children[i])
			try(check_snapshot_edge(trans, &s, k.k->p.offset,
						EDGE_PARENT, le32_to_cpu(s.children[i])));

	struct bch_snapshot parent = {};
	u32 parent_id = le32_to_cpu(s.parent);
	if (parent_id)
		try(bch2_snapshot_lookup(trans, parent_id, &parent));

	ret = snapshot_tree_ptr_good(trans, k.k->p.offset, le32_to_cpu(s.tree));
	if (ret < 0)
		return ret;

	if (ret_fsck_err_on(!ret,
			trans, snapshot_to_bad_snapshot_tree,
			"snapshot points to missing/incorrect tree:\n%s",
			(bch2_bkey_val_to_text(&buf, c, k), buf.buf)))
		try(snapshot_tree_ptr_repair(trans, iter, k, &s));
	ret = 0;

	u32 real_depth = parent_id ? le32_to_cpu(parent.depth) + 1 : 0;

	if (ret_fsck_err_on(le32_to_cpu(s.depth) != real_depth,
			trans, snapshot_bad_depth,
			"snapshot with incorrect depth field, should be %u:\n%s",
			real_depth, (bch2_bkey_val_to_text(&buf, c, k), buf.buf))) {
		u = u ?: errptr_try(bch2_bkey_make_mut_typed(trans, iter, &k, 0, snapshot));

		u->v.depth = cpu_to_le32(real_depth);
		s = u->v;
	}

	for (unsigned i = 0; i < 3; i++) {
		u32 skip = le32_to_cpu(s.skip[i]);

		bool bad = !s.parent
			? skip
			: !bch2_snapshot_is_ancestor_early(c, k.k->p.offset, skip);

		if (bad) {
			printbuf_reset(&buf);

			prt_printf(&buf, "snapshot with bad skiplist pointer %u:\n", skip);
			bch2_bkey_val_to_text(&buf, c, k);
			prt_newline(&buf);

			if (skip) {
				prt_printf(&buf, "points to\n  ");

				CLASS(btree_iter, skip_iter)(trans, BTREE_ID_snapshots, POS(0, skip), 0);
				struct bkey_s_c skip_k = bkey_try(bch2_btree_iter_peek_slot(&skip_iter));

				bch2_bkey_val_to_text(&buf, c, skip_k);
				prt_newline(&buf);
			}

			if (ret_fsck_err(trans, snapshot_bad_skiplist, "%s", buf.buf)) {
				u = u ?: errptr_try(bch2_bkey_make_mut_typed(trans, iter, &k, 0, snapshot));
				u->v.skip[i] = cpu_to_le32(bch2_snapshot_skiplist_get(c, parent_id));
			}
		}
	}

	if (u)
		bubble_sort(u->v.skip, ARRAY_SIZE(u->v.skip), cmp_le32);

	try(check_snapshot_to_subvol(trans, iter, k, &s, u));

	return 0;
}

int bch2_check_snapshots_trans(struct btree_trans *trans)
{
	/*
	 * We iterate backwards as checking/fixing the depth field requires that
	 * the parent's depth already be correct:
	 */
	try(for_each_btree_key_reverse_commit(trans, iter,
				BTREE_ID_snapshots, POS_MAX,
				BTREE_ITER_prefetch, k,
				NULL, NULL, BCH_TRANS_COMMIT_no_enospc,
			check_snapshot(trans, &iter, k)));

	if (trans->c->snapshots.need_table_rebuild)
		try(bch2_snapshot_table_rebuild(trans));

	return 0;
}


int bch2_check_snapshots(struct bch_fs *c)
{
	/*
	 * We iterate backwards as checking/fixing the depth field requires that
	 * the parent's depth already be correct:
	 */
	CLASS(btree_trans, trans)(c);
	return bch2_check_snapshots_trans(trans);
}

static int check_snapshot_exists(struct btree_trans *trans, u32 id)
{
	struct bch_fs *c = trans->c;

	/* Do we need to reconstruct the snapshot_tree entry as well? */
	struct bkey_s_c k;
	int ret = 0;
	u32 tree_id = 0;

	for_each_btree_key_norestart(trans, iter, BTREE_ID_snapshot_trees, POS_MIN,
				     0, k, ret) {
		if (k.k->type == KEY_TYPE_snapshot_tree &&
		    le32_to_cpu(bkey_s_c_to_snapshot_tree(k).v->root_snapshot) == id) {
			tree_id = k.k->p.offset;
			break;
		}
	}

	if (ret)
		return ret;

	if (!tree_id)
		try(bch2_snapshot_tree_create(trans, id, 0, &tree_id));

	struct bkey_i_snapshot *snapshot = bch2_trans_kmalloc(trans, sizeof(*snapshot));
	ret = PTR_ERR_OR_ZERO(snapshot);
	if (ret)
		return ret;

	bkey_snapshot_init(&snapshot->k_i);
	snapshot->k.p		= POS(0, id);
	snapshot->v.tree	= cpu_to_le32(tree_id);
	snapshot->v.btime.lo	= cpu_to_le64(bch2_current_time(c));

	for_each_btree_key_norestart(trans, iter, BTREE_ID_subvolumes, POS_MIN,
				     0, k, ret) {
		if (k.k->type == KEY_TYPE_subvolume &&
		    le32_to_cpu(bkey_s_c_to_subvolume(k).v->snapshot) == id) {
			snapshot->v.subvol = cpu_to_le32(k.k->p.offset);
			break;
		}
	}

	bch2_snapshot_state_set(&snapshot->v, SNAPSHOT_STATE_live);

	return  bch2_snapshot_table_make_room(c, id) ?:
		bch2_btree_insert_trans(trans, BTREE_ID_snapshots, &snapshot->k_i, 0);
}

/* Figure out which snapshot nodes belong in the same tree: */
struct snapshot_tree_reconstruct {
	enum btree_id			btree;
	struct bpos			cur_pos;
	snapshot_id_list		cur_ids;
	DARRAY(snapshot_id_list)	trees;
};

static void snapshot_tree_reconstruct_exit(struct snapshot_tree_reconstruct *r)
{
	darray_for_each(r->trees, i)
		darray_exit(i);
	darray_exit(&r->trees);
	darray_exit(&r->cur_ids);
}

static inline bool same_snapshot(struct snapshot_tree_reconstruct *r, struct bpos pos)
{
	return r->btree == BTREE_ID_inodes
		? r->cur_pos.offset == pos.offset
		: r->cur_pos.inode == pos.inode;
}

static inline bool snapshot_id_lists_have_common(snapshot_id_list *l, snapshot_id_list *r)
{
	return darray_find_p(*l, i, snapshot_list_has_id(r, *i)) != NULL;
}

static int snapshot_tree_reconstruct_next(struct bch_fs *c, struct snapshot_tree_reconstruct *r)
{
	if (r->cur_ids.nr) {
		darray_for_each(r->trees, i)
			if (snapshot_id_lists_have_common(i, &r->cur_ids)) {
				try(snapshot_list_merge(c, i, &r->cur_ids));
				r->cur_ids.nr = 0;
				return 0;
			}
		darray_push(&r->trees, r->cur_ids);
		darray_init(&r->cur_ids);
	}

	return 0;
}

static int get_snapshot_trees(struct bch_fs *c, struct snapshot_tree_reconstruct *r, struct bpos pos)
{
	if (!same_snapshot(r, pos))
		snapshot_tree_reconstruct_next(c, r);
	r->cur_pos = pos;
	return snapshot_list_add_nodup(c, &r->cur_ids, pos.snapshot);
}

int bch2_reconstruct_snapshots(struct bch_fs *c)
{
	CLASS(btree_trans, trans)(c);
	CLASS(printbuf, buf)();
	struct snapshot_tree_reconstruct r __cleanup(snapshot_tree_reconstruct_exit) = {};
	int ret = 0;

	struct progress_indicator progress;
	bch2_progress_init(&progress, __func__, c, btree_has_snapshots_mask, 0);

	for (unsigned btree = 0; btree < BTREE_ID_NR; btree++) {
		if (btree_type_has_snapshots(btree)) {
			r.btree = btree;

			try(for_each_btree_key(trans, iter, btree, POS_MIN,
					BTREE_ITER_all_snapshots|BTREE_ITER_prefetch, k, ({
				bch2_progress_update_iter(trans, &progress, &iter) ?:
				get_snapshot_trees(c, &r, k.k->p);
			})));

			snapshot_tree_reconstruct_next(c, &r);
		}
	}

	darray_for_each(r.trees, t) {
		printbuf_reset(&buf);
		bch2_snapshot_id_list_to_text(&buf, t);

		darray_for_each(*t, id) {
			if (fsck_err_on(bch2_snapshot_id_state(c, *id) == SNAPSHOT_ID_empty,
					trans, snapshot_node_missing,
					"snapshot node %u from tree %s missing, recreate?", *id, buf.buf)) {
				if (t->nr > 1) {
					bch_err(c, "cannot reconstruct snapshot trees with multiple nodes");
					return bch_err_throw(c, fsck_repair_unimplemented);
				}

				try(commit_do(trans, NULL, NULL, BCH_TRANS_COMMIT_no_enospc,
					      check_snapshot_exists(trans, *id)));
			}
		}
	}
fsck_err:
	return ret;
}

int __bch2_check_key_has_snapshot(struct btree_trans *trans,
				  struct btree_iter *iter,
				  struct bkey_s_c k)
{
	struct bch_fs *c = trans->c;
	CLASS(printbuf, buf)();
	int ret = 0;
	enum snapshot_id_state state = bch2_snapshot_id_state(c, k.k->p.snapshot);

	/* promote path - can't do repair */
	if (!iter)
		return 1;

	/*
	 * Snapshot was deleted. If there's no live descendant (a leaf, or an
	 * interior node whose subtree is entirely deleted) the key is genuinely
	 * orphaned - nothing can see it - so delete it. If there is a live
	 * descendant the key is still visible to it via inheritance and should
	 * have been migrated there during deletion; migrate it now rather than
	 * dropping it (the deleted node retains a child pointer so
	 * bch2_snapshot_live_descendent() can find the target). Both autofix.
	 */
	if (state == SNAPSHOT_ID_deleted) {
		u32 live_child = bch2_snapshot_live_descendent(c, k.k->p.snapshot);

		if (fsck_err_on(!live_child,
				trans, bkey_in_deleted_snapshot,
				"key in deleted snapshot %s, delete?",
				(bch2_btree_id_to_text(&buf, iter->btree_id),
				 prt_char(&buf, ' '),
				 bch2_bkey_val_to_text(&buf, c, k), buf.buf)))
			ret = bch2_btree_delete_at(trans, iter,
						   BTREE_UPDATE_internal_snapshot_node) ?: 1;

		if (fsck_err_on(live_child,
				trans, bkey_in_deleted_interior_snapshot,
				"key in deleted interior snapshot %s, migrate to live descendant?",
				(bch2_btree_id_to_text(&buf, iter->btree_id),
				 prt_char(&buf, ' '),
				 bch2_bkey_val_to_text(&buf, c, k), buf.buf)))
			ret = bch2_delete_dead_snapshot_key(trans, iter, k, live_child) ?: 1;
	}

	if (state == SNAPSHOT_ID_empty) {
		/*
		 * Snapshot missing: we should have caught this with btree_lost_data and
		 * kicked off reconstruct_snapshots, so if we end up here we have no
		 * idea what happened.
		 *
		 * Do not delete unless we know that subvolumes and snapshots
		 * are consistent:
		 *
		 * XXX:
		 *
		 * We could be smarter here, and instead of using the generic
		 * recovery pass ratelimiting, track if there have been any
		 * changes to the snapshots or inodes btrees since those passes
		 * last ran.
		 */
		ret = bch2_require_recovery_pass(c, &buf, BCH_RECOVERY_PASS_check_snapshots) ?: ret;
		ret = bch2_require_recovery_pass(c, &buf, BCH_RECOVERY_PASS_check_subvols) ?: ret;

		if (c->sb.btrees_lost_data & BIT_ULL(BTREE_ID_snapshots))
			ret = bch2_require_recovery_pass(c, &buf, BCH_RECOVERY_PASS_reconstruct_snapshots) ?: ret;

		unsigned repair_flags = FSCK_CAN_IGNORE | (!ret ? FSCK_CAN_FIX : 0);

		if (__fsck_err(trans, repair_flags, bkey_in_missing_snapshot,
			     "key in missing snapshot %s, delete?",
			     (bch2_btree_id_to_text(&buf, iter->btree_id),
			      prt_char(&buf, ' '),
			      bch2_bkey_val_to_text(&buf, c, k), buf.buf))) {
			ret = bch2_btree_delete_at(trans, iter,
						   BTREE_UPDATE_internal_snapshot_node) ?: 1;
		}
	}
fsck_err:
	return ret;
}
