// SPDX-License-Identifier: GPL-2.0
/*
 * Snapshot fsck: the passes over the snapshots and snapshot_trees btrees.
 *
 * check_snapshot_trees: every tree key names a live root and master subvol.
 *
 * check_snapshots: per-node, in three stages - check_snapshot_state()
 * recovers the state field itself; check_snapshot_deleted() holds a
 * non-live state up against its witnesses; then the topology checks
 * (edges, tree pointer, depth, skiplists, subvol backref).
 *
 * reconstruct_snapshots: rebuild missing snapshot nodes from the keys
 * that reference them.
 *
 * __bch2_check_key_has_snapshot: per-key repair for keys whose snapshot
 * node is missing or dead; also called from runtime paths.
 *
 * Repair philosophy: enumerate which writers can produce a state before
 * repairing it. Unconstructible combinations are rejected at commit
 * (bch2_snapshot_validate()); real damage is repaired toward the side
 * the witnesses corroborate - child snapshots, the subvolume (deletion
 * tombstones it in the same transaction that condemns its snapshot),
 * the per-snapshot accounting (nothing we write deletes a node with
 * data). Ambiguity fail-stops rather than guessing.
 */
#include "bcachefs.h"

#include "alloc/accounting.h"

#include "btree/cache.h"
#include "btree/update.h"

#include "fs/inode.h"

#include "snapshots/snapshot.h"
#include "snapshots/subvolume.h"

#include "init/error.h"
#include "init/passes.h"
#include "init/progress.h"
#include "init/recovery.h"

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

/* check_snapshot_trees: */

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
/* check_snapshots: */

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

/* Find a subvolume claiming snapshot @id, to restore a wiped backref: */
static int subvol_claiming_snapshot(struct btree_trans *trans, u32 id,
				    u32 *subvol_id)
{
	struct bkey_s_c k;
	int ret;

	for_each_btree_key_norestart(trans, iter, BTREE_ID_subvolumes, POS_MIN,
				     0, k, ret)
		if (k.k->type == KEY_TYPE_subvolume &&
		    le32_to_cpu(bkey_s_c_to_subvolume(k).v->snapshot) == id) {
			*subvol_id = k.k->p.offset;
			break;
		}

	return ret;
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

	if (s->subvol) {
		u32 id = le32_to_cpu(s->subvol);

		/*
		 * Raw read: bch2_subvolume_get() reports deleted subvolumes
		 * as ENOENT, and the message should show what's actually
		 * there:
		 */
		CLASS(btree_iter, subvol_iter)(trans, BTREE_ID_subvolumes, POS(0, id), 0);
		struct bkey_s_c_subvolume subvol_k = bch2_bkey_get_typed(&subvol_iter, subvolume);
		int ret = bkey_err(subvol_k);
		if (ret && !bch2_err_matches(ret, ENOENT))
			return ret;

		struct bch_subvolume subvol = {};
		if (!ret)
			bkey_val_copy_pad(&subvol, subvol_k);

		bool snap_deleting	= bch2_snapshot_state(s) == SNAPSHOT_STATE_will_delete;
		bool subvol_deleted	= !ret &&
			bch2_subvolume_state_compat(&subvol) == SUBVOLUME_STATE_deleted;
		bool points_back	= !ret &&
			le32_to_cpu(subvol.snapshot) == k.k->p.offset;

		if (ret || !points_back) {
			/*
			 * Missing subvolume or wrong backref: repair needs
			 * the subvolume side validated first - it belongs to
			 * the dedicated pass after check_subvols. Report
			 * only; an error return here would regress mounts of
			 * filesystems mid-deletion:
			 */
			CLASS(bch_log_msg, msg)(c);

			if (ret)
				prt_printf(&msg.m, "snapshot points to missing subvolume %u:\n", id);
			else
				prt_printf(&msg.m, "snapshot's subvolume doesn't point back at it:\n");
			bch2_bkey_val_to_text(&msg.m, c, k);
			if (!ret) {
				prt_newline(&msg.m);
				bch2_bkey_val_to_text(&msg.m, c, subvol_k.s_c);
			}
			msg.m.suppress = !bch2_count_fsck_err(c, snapshot_subvol_backref_wrong, &msg.m);
			return 0;
		}

		/*
		 * The deletion machinery couples exactly one bit on each
		 * side: a snapshot is will_delete iff its subvolume is
		 * tombstoned (live vs unlinked is the subvolume's own
		 * user-visibility business, invisible to the snapshot).
		 * With the edge intact, a state mismatch repairs in one
		 * direction only: the subvolume implies the snapshot state
		 * exactly, while the reverse would have to guess between
		 * live and unlinked.
		 */
		if (ret_fsck_err_on(snap_deleting != subvol_deleted,
				    trans, snapshot_subvol_state_mismatch,
				    "snapshot %s but its subvolume is %s:\n%s",
				    snap_deleting ? "will_delete" : "live",
				    subvol_deleted ? "deleted" : "not deleted",
				    (printbuf_reset(&buf),
				     bch2_bkey_val_to_text(&buf, c, k),
				     prt_newline(&buf),
				     bch2_bkey_val_to_text(&buf, c, subvol_k.s_c),
				     buf.buf))) {
			u = u ?: errptr_try(bch2_bkey_make_mut_typed(trans, iter, &k, 0, snapshot));
			bch2_snapshot_state_set(&u->v,
						subvol_deleted
						? SNAPSHOT_STATE_will_delete
						: SNAPSHOT_STATE_live);
			*s = u->v;
		}
	} else if (should_have_subvol &&
		   bch2_snapshot_state(s) == SNAPSHOT_STATE_live) {
		/*
		 * A live leaf with no backref: a subvolume still pointing at
		 * it means the backref was wiped - restore it. (A second
		 * claimant, if damage minted one, still hits check_subvols'
		 * doesn't-point-back fail-stop.) No claimant is an orphan
		 * leaf, whose repair (creating a subvolume) is
		 * unimplemented, as above.
		 */
		u32 subvol_id = 0;
		try(subvol_claiming_snapshot(trans, k.k->p.offset, &subvol_id));

		if (subvol_id &&
		    ret_fsck_err(trans, snapshot_subvol_backref_wrong,
				 "snapshot leaf missing subvol backref, subvolume %u points at it - restoring:\n%s",
				 subvol_id,
				 (bch2_bkey_val_to_text(&buf, c, k), buf.buf))) {
			u = u ?: errptr_try(bch2_bkey_make_mut_typed(trans, iter, &k, 0, snapshot));
			u->v.subvol = cpu_to_le32(subvol_id);
			SET_BCH_SNAPSHOT_SUBVOL_OBSOLETE(&u->v, true);
			*s = u->v;
		}
	}

	if (ret_fsck_err_on(s->subvol && !should_have_subvol,
			    trans, snapshot_should_not_have_subvol,
			    "snapshot should not point to subvol:\n%s",
			    (bch2_bkey_val_to_text(&buf, c, k), buf.buf))) {
		if (s->children[0])
			return bch_err_throw(c, fsck_repair_unimplemented);

		u = u ?: errptr_try(bch2_bkey_make_mut_typed(trans, iter, &k, 0, snapshot));

		/* XXX: DANGEROUS */

		u->v.subvol = 0;
		*s = u->v;
	}

	if (BCH_SNAPSHOT_SUBVOL_OBSOLETE(s) != (s->subvol != 0)) {
		printbuf_reset(&buf);
		prt_printf(&buf, "snapshot node %llu has wrong subvol flag:\n",
			   k.k->p.offset);
		bch2_bkey_val_to_text(&buf, c, k);

		if (s->subvol) {
			CLASS(btree_iter, subvol_iter)(trans, BTREE_ID_subvolumes,
						       POS(0, le32_to_cpu(s->subvol)), 0);
			struct bkey_s_c subvol_k = bkey_try(bch2_btree_iter_peek_slot(&subvol_iter));

			prt_newline(&buf);
			bch2_bkey_val_to_text(&buf, c, subvol_k);
		}

		if (ret_fsck_err(trans, snapshot_subvol_flag_wrong, "%s", buf.buf)) {
			u = u ?: errptr_try(bch2_bkey_make_mut_typed(trans, iter, &k, 0, snapshot));

			SET_BCH_SNAPSHOT_SUBVOL_OBSOLETE(&u->v, s->subvol != 0);
			*s = u->v;
		}
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

/* snapshot edge repair: */

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

	/* btree 0 (extents, the default) is the only one with external_sectors (counter 2) */
	u64 v[3] = {};
	bch2_accounting_mem_read(c, disk_accounting_pos_to_bpos(&acc), v, ARRAY_SIZE(v));
	return v[2];
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

	/*
	 * Connectivity: the live tree must be closed over not-deleted nodes.
	 * A deleted target is history - an interrupted deletion left our edge
	 * pointing into it - so rewrite our own edge past it: parent edges
	 * walk up the tombstone's retained parent chain, child edges walk
	 * down retained children, to the nearest not-deleted node (or
	 * nothing, if that direction is all dead). The tombstone itself is
	 * untouched; depth/skiplist fallout is repaired by the checks
	 * downstream. (A tombstone with two live children was revived by
	 * check_snapshot_deleted() before we got here, so walking
	 * children[0] doesn't skip a live sibling.)
	 */
	if (other_exists &&
	    bch2_snapshot_state_compat(&other) == SNAPSHOT_STATE_deleted) {
		u32 repl = other_id;
		struct bch_snapshot t = other;

		for (unsigned iters = 0; ; iters++) {
			if (iters > BTREE_MAX_DEPTH * 64) /* damaged chain, cycle? don't guess */
				return 0;

			repl = le32_to_cpu(side == EDGE_CHILD ? t.parent : t.children[0]);
			if (!repl)
				break;

			int ret2 = bch2_snapshot_lookup(trans, repl, &t);
			if (bch2_err_matches(ret2, ENOENT)) {
				repl = 0;
				break;
			}
			if (ret2)
				return ret2;

			if (bch2_snapshot_state_compat(&t) != SNAPSHOT_STATE_deleted)
				break;
		}

		if (ret_fsck_err(trans, snapshot_deleted_but_linked,
				 "snapshot %u %s pointer %u is a deleted node - %s %u",
				 id, side == EDGE_CHILD ? "parent" : "child", other_id,
				 repl ? "re-linking past it to" : "clearing, dead in that direction:",
				 repl))
			return snapshot_edge_set_ptr(trans, id, side, other_id, repl);
		return 0;
	}

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

/*
 * Is anything still pointing at this snapshot node - a subvolume whose
 * snapshot is us, our parent listing us as a child, or a child naming us as
 * its parent? Used to recover a node whose state field is garbage: if it's
 * referenced it must be live, whatever the corrupted state says.
 */
static int snapshot_referenced(struct btree_trans *trans,
			       const struct bch_snapshot *s, u32 id, bool *ref)
{
	*ref = true;

	if (s->subvol) {
		struct bch_subvolume subvol;
		int ret = bch2_subvolume_get(trans, le32_to_cpu(s->subvol), false, &subvol);
		if (ret && !bch2_err_matches(ret, ENOENT))
			return ret;
		if (!ret && le32_to_cpu(subvol.snapshot) == id)
			return 0;
	}

	if (s->parent) {
		struct bch_snapshot p;
		int ret = bch2_snapshot_lookup(trans, le32_to_cpu(s->parent), &p);
		if (ret && !bch2_err_matches(ret, ENOENT))
			return ret;
		if (!ret && snapshot_node_points_back(&p, EDGE_PARENT, id))
			return 0;
	}

	for (unsigned i = 0; i < 2; i++) {
		if (!s->children[i])
			continue;

		struct bch_snapshot ch;
		int ret = bch2_snapshot_lookup(trans, le32_to_cpu(s->children[i]), &ch);
		if (ret && !bch2_err_matches(ret, ENOENT))
			return ret;
		if (!ret && snapshot_node_points_back(&ch, EDGE_CHILD, id))
			return 0;
	}

	*ref = false;
	return 0;
}

/*
 * Recover the state field itself: stamp it from the legacy flags when
 * unset, decode a corrupted value back to the nearest codeword, and
 * correct a state left stale by a legacy flags-only tombstone.
 */
static int check_snapshot_state(struct btree_trans *trans,
				struct btree_iter *iter,
				struct bkey_s_c k,
				struct bch_snapshot *s,
				struct bkey_i_snapshot **u)
{
	struct bch_fs *c = trans->c;
	CLASS(printbuf, buf)();

	/*
	 * A zero state field means the key predates the state field, or was
	 * wiped: the legacy flag bits are then authoritative (old kernels
	 * dual-wrote them). Derive the state from the bits whenever it's unset,
	 * regardless of upgrade status - so a fs already on the new version but
	 * carrying pre-state keys (never migrated, because no upgrade transition
	 * ran) heals too. Mid-upgrade this is the expected migration and silent;
	 * post-upgrade an unset state is unexpected, so surface it (autofix).
	 */
	if (!bch2_snapshot_state(s)) {
		bool upgrading = c->sb.version_upgrade_complete <
			bcachefs_metadata_version_per_dev_fragmentation_lru;
		if (upgrading ||
		    ret_fsck_err(trans, snapshot_state_bad,
				 "snapshot state unset, recovering from legacy flags:\n%s",
				 (bch2_bkey_val_to_text(&buf, c, k), buf.buf))) {
			*u = *u ?: errptr_try(bch2_bkey_make_mut_typed(trans, iter, &k, 0, snapshot));
			(*u)->v.state = cpu_to_le32(bch2_snapshot_state_from_flags(s));
			*s = (*u)->v;
		}
	}

	/*
	 * Pre-upgrade, the rewrite above always leaves a valid state, so this
	 * only fires post-upgrade - where any invalid value (including zero)
	 * is corruption. No repair yet, and state-keyed repairs must not run
	 * on a state we can't read:
	 */
	if (!bch2_snapshot_state_valid(bch2_snapshot_state(s))) {
		unsigned dist;
		enum bch_snapshot_state nearest =
			bch2_snapshot_state_nearest(le32_to_cpu(s->state), &dist);

		/*
		 * Codewords are >= 14 apart, so anything <= 6 bits out is
		 * uniquely decodable and recoverable. <= 2 bits is a genuine
		 * bitflip (its own error, so failing hardware shows up in the
		 * counters); 3-6 is larger but still-recoverable corruption.
		 * Further out is not a bitflip we can trust: fail-stop.
		 */
		if (dist <= 2) {
			if (ret_fsck_err(trans, snapshot_state_bitflip,
					 "snapshot state 0x%x is a %u-bit flip of %s - correcting:\n%s",
					 le32_to_cpu(s->state), dist,
					 bch2_snapshot_state_str(nearest),
					 (bch2_bkey_val_to_text(&buf, c, k), buf.buf))) {
				*u = *u ?: errptr_try(bch2_bkey_make_mut_typed(trans, iter, &k, 0, snapshot));
				bch2_snapshot_state_set(&(*u)->v, nearest);
				*s = (*u)->v;
			}
		} else if (dist <= 6) {
			if (ret_fsck_err(trans, snapshot_state_bad,
					 "snapshot state 0x%x is %u bits from %s - correcting:\n%s",
					 le32_to_cpu(s->state), dist,
					 bch2_snapshot_state_str(nearest),
					 (bch2_bkey_val_to_text(&buf, c, k), buf.buf))) {
				*u = *u ?: errptr_try(bch2_bkey_make_mut_typed(trans, iter, &k, 0, snapshot));
				bch2_snapshot_state_set(&(*u)->v, nearest);
				*s = (*u)->v;
			}
		} else {
			/*
			 * Too far from any codeword to decode. But the rest of
			 * the key is intact, so if anything still references
			 * this node it must be live - mark it live. An
			 * unreferenced garbage node we can't place: fail-stop.
			 */
			bool ref;
			int ret2 = snapshot_referenced(trans, s, k.k->p.offset, &ref);
			if (ret2)
				return ret2;

			if (ref) {
				if (ret_fsck_err(trans, snapshot_state_bad,
						 "snapshot state 0x%x is garbage, but the node is referenced - marking live:\n%s",
						 le32_to_cpu(s->state),
						 (bch2_bkey_val_to_text(&buf, c, k), buf.buf))) {
					*u = *u ?: errptr_try(bch2_bkey_make_mut_typed(trans, iter, &k, 0, snapshot));
					bch2_snapshot_state_set(&(*u)->v, SNAPSHOT_STATE_live);
					*s = (*u)->v;
				}
			} else {
				CLASS(bch_log_msg, msg)(c);

				prt_printf(&msg.m, "snapshot has invalid state 0x%x (nearest codeword %s is %u bits away, node unreferenced):\n",
					   le32_to_cpu(s->state), bch2_snapshot_state_str(nearest), dist);
				bch2_bkey_val_to_text(&msg.m, c, k);
				msg.m.suppress = !bch2_count_fsck_err(c, snapshot_state_bad, &msg.m);

				return bch_err_throw(c, fsck_repair_unimplemented);
			}
		}
	}

	/*
	 * A valid state that contradicts the legacy flags: current writers
	 * dual-write both via bch2_snapshot_state_set(), so only a legacy
	 * flags-only writer leaves them disagreeing - the flags are the fresher
	 * write. But the flags are single unprotected bits, so corroborate
	 * structurally before trusting them over a codeword: a node tombstoned
	 * by a legacy bch2_snapshot_node_delete() carries the tombstone wipe,
	 * and no live, will_delete or no_keys node ever has tree == 0. A bare
	 * DELETED bit on a live-shaped node stays with the state field.
	 *
	 * Left as-is, the interior-deletion collector reads the stale no_keys
	 * state, queues the tombstone, and the breadcrumb child pointer
	 * fail-stops the edge checks - wedging every mount (the 2026-07-20
	 * field report; snapshot-inject/stale_state_tombstone).
	 */
	if (bch2_snapshot_state(s) != SNAPSHOT_STATE_deleted &&
	    BCH_SNAPSHOT_DELETED_OBSOLETE(s) &&
	    !s->tree &&
	    ret_fsck_err(trans, snapshot_state_stale_tombstone,
			 "snapshot spliced out by a legacy kernel (tombstone shape, legacy deleted flag)\n"
			 "but the state field is stale at %s - correcting to deleted:\n%s",
			 bch2_snapshot_state_str(bch2_snapshot_state(s)),
			 (bch2_bkey_val_to_text(&buf, c, k), buf.buf))) {
		*u = *u ?: errptr_try(bch2_bkey_make_mut_typed(trans, iter, &k, 0, snapshot));
		bch2_snapshot_state_set(&(*u)->v, SNAPSHOT_STATE_deleted);
		*s = (*u)->v;
	}

	return 0;
}

/*
 * A non-live state, checked against its witnesses - child snapshots,
 * the subvolume, the accounting. Returns 1 if the node was deleted or
 * is a settled tombstone: no further checking.
 */
static int check_snapshot_deleted(struct btree_trans *trans,
				  struct btree_iter *iter,
				  struct bkey_s_c k,
				  struct bch_snapshot *s,
				  struct bkey_i_snapshot **u)
{
	struct bch_fs *c = trans->c;
	CLASS(printbuf, buf)();

	/*
	 * A non-live node with two live children that both point back at it is a
	 * lie: it's a branching interior that two subtrees still depend on as
	 * their common ancestor, so it can't be gone. Reviving it is the simplest
	 * repair back to a consistent tree.
	 *
	 * A single live child is legal, and does not trigger this: a no_keys node
	 * is an emptied interior kept until the next remount, and it retains its
	 * one live descendant so ancestry still resolves - non-live nodes are
	 * single-child by construction.
	 *
	 * Children must reciprocate. A deleted node's child pointer can be a
	 * splice breadcrumb (I1): a child reparented to the grandparent that no
	 * longer names us as parent doesn't depend on us and isn't counted.
	 *
	 * Do this before the deleted early-out, then fall through so the
	 * edge/depth/tree checks validate the now-live node.
	 */
	if (bch2_snapshot_state(s) != SNAPSHOT_STATE_live) {
		unsigned nr_live_children = 0;

		for (unsigned i = 0; i < 2; i++) {
			if (!s->children[i])
				continue;

			struct bch_snapshot child;
			int ret2 = bch2_snapshot_lookup(trans, le32_to_cpu(s->children[i]), &child);
			if (ret2 && !bch2_err_matches(ret2, ENOENT))
				return ret2;

			nr_live_children += !ret2 &&
				bch2_snapshot_state_compat(&child) == SNAPSHOT_STATE_live &&
				snapshot_node_points_back(&child, EDGE_CHILD, k.k->p.offset);
		}

		if (ret_fsck_err_on(nr_live_children == 2,
				trans, snapshot_deleted_has_live_children,
				"snapshot marked %s but has two live children - reviving:\n%s",
				bch2_snapshot_state_str(bch2_snapshot_state(s)),
				(bch2_bkey_val_to_text(&buf, c, k), buf.buf))) {
			*u = *u ?: errptr_try(bch2_bkey_make_mut_typed(trans, iter, &k, 0, snapshot));
			bch2_snapshot_state_set(&(*u)->v, SNAPSHOT_STATE_live);
			*s = (*u)->v;
		}

		/*
		 * Same check via the subvolume: deletion tombstones the
		 * subvolume in the same transaction that marks its snapshot,
		 * so a non-live leaf whose subvolume is live and points back
		 * has a bad state field. This also fixes, in the same pass, a
		 * bad state stamped by the zero-state migration above from a
		 * corrupt legacy flag. (A tombstoned subvolume reads as
		 * ENOENT here: that's normal mid-deletion, not evidence.)
		 */
		if (!s->children[0] && s->subvol) {
			struct bch_subvolume subvol;
			int ret2 = bch2_subvolume_get(trans, le32_to_cpu(s->subvol),
						      false, &subvol);
			if (ret2 && !bch2_err_matches(ret2, ENOENT))
				return ret2;

			if (!ret2 &&
			    bch2_subvolume_state_compat(&subvol) == SUBVOLUME_STATE_live &&
			    le32_to_cpu(subvol.snapshot) == k.k->p.offset &&
			    ret_fsck_err(trans, snapshot_deleted_but_subvol_live,
					 "snapshot marked %s but its subvolume is live - reviving:\n%s",
					 bch2_snapshot_state_str(bch2_snapshot_state(s)),
					 (bch2_bkey_val_to_text(&buf, c, k), buf.buf))) {
				*u = *u ?: errptr_try(bch2_bkey_make_mut_typed(trans, iter, &k, 0, snapshot));
				bch2_snapshot_state_set(&(*u)->v, SNAPSHOT_STATE_live);
				*s = (*u)->v;
			}
		}
	}

	/*
	 * Every deleted node gets held up against the accounting: nothing we
	 * write deletes a node with data still accounted to it, so data means
	 * the state field is the lie, whatever the rest of the node says -
	 * undelete, splicing the node back into the tree (the tombstone
	 * retained its pointers), and fall through so the edge checks
	 * validate the result. No data: settled tombstone, nothing to do.
	 */
	if (bch2_snapshot_state(s) == SNAPSHOT_STATE_deleted) {
		u64 keys, sectors;
		bch2_snapshot_accounting_totals(c, k.k->p.offset, &keys, &sectors, NULL);

		if (ret_fsck_err_on(keys || sectors,
				trans, snapshot_deleted_but_has_data,
				"deleted snapshot node has %llu keys / %llu sectors accounted - undeleting:\n%s",
				keys, sectors,
				(printbuf_reset(&buf),
				 bch2_bkey_val_to_text(&buf, c, k), buf.buf))) {
			*u = *u ?: errptr_try(bch2_bkey_make_mut_typed(trans, iter, &k, 0, snapshot));

			/*
			 * Relinking rewrites pointers on nodes this pass may
			 * already have verified - require another run:
			 */
			bool relinked = false;
			try(bch2_snapshot_node_undelete(trans, *u, &relinked));
			if (relinked)
				try(bch2_require_recovery_pass(c, &buf,
						BCH_RECOVERY_PASS_check_snapshots));
			*s = (*u)->v;
		}
	}

	return bch2_snapshot_state(s) == SNAPSHOT_STATE_deleted;
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
	try(check_snapshot_state(trans, iter, k, &s, &u));

	ret = check_snapshot_deleted(trans, iter, k, &s, &u);
	if (ret)
		return ret < 0 ? ret : 0;


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
	int ret = bch2_check_snapshots_trans(trans);

	/*
	 * If the pass completed cleanly the snapshots btree is consistent;
	 * record it so check_key_has_snapshot can trust the in-memory snapshot
	 * table (see bch2_btree_is_clean). This is the same gate the pass runner
	 * uses to mark a pass complete.
	 */
	if (!ret && !test_bit(BCH_FS_error, &c->flags))
		bch2_set_btree_clean(c, BTREE_ID_snapshots);
	return ret;
}

/* reconstruct_snapshots: */

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

	u32 subvol_id = 0;
	try(subvol_claiming_snapshot(trans, id, &subvol_id));
	snapshot->v.subvol = cpu_to_le32(subvol_id);

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

/*
 * Recreate one missing snapshot node, committed as a single unit: the
 * fsck_err queues its journal log entry and check_snapshot_exists() queues the
 * new node, and commit_do() commits both. Kept inside the commit_do() body (a
 * lockrestart_do, which begins with bch2_trans_begin) so the queued log isn't
 * discarded before the commit - the caller must not queue it outside.
 */
static int reconstruct_snapshot_node(struct btree_trans *trans, u32 id,
				     unsigned tree_nr, struct printbuf *buf)
{
	struct bch_fs *c = trans->c;

	if (ret_fsck_err_on(bch2_snapshot_id_state(c, id) == SNAPSHOT_ID_empty,
			    trans, snapshot_node_missing,
			    "snapshot node %u from tree %s missing, recreate?", id, buf->buf)) {
		if (tree_nr > 1) {
			bch_err(c, "cannot reconstruct snapshot trees with multiple nodes");
			return bch_err_throw(c, fsck_repair_unimplemented);
		}

		return check_snapshot_exists(trans, id);
	}

	return 0;
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

		darray_for_each(*t, id)
			try(commit_do(trans, NULL, NULL, BCH_TRANS_COMMIT_no_enospc,
				      reconstruct_snapshot_node(trans, *id, t->nr, &buf)));
	}

	return ret;
}

/*
 * When we migrate a key out of a deleted snapshot to its live descendant, the
 * snapshot-deletion scan's premise - "a key in snapshot X implies an inode in
 * snapshot X" - has to hold at the destination too, or the key gets stranded
 * again on the next deletion. If the inode is only inherited from an ancestor
 * of the descendant, copy it down. A genuinely missing inode is left for a
 * full fsck to reconstruct; we don't do that or schedule passes here.
 */
/* check_key_has_snapshot - per-key repair, also called at runtime: */

static int check_key_has_inode_in_snapshot(struct btree_trans *trans,
					   enum btree_id btree, u64 inum, u32 snapshot)
{
	switch (btree) {
	case BTREE_ID_extents:
	case BTREE_ID_dirents:
	case BTREE_ID_xattrs:
		break;
	default:
		return 0;
	}

	struct bch_inode_unpacked inode;
	int ret = bch2_inode_find_by_inum_snapshot(trans, inum, snapshot, &inode, 0);
	if (ret)
		return bch2_err_matches(ret, ENOENT) ? 0 : ret;

	if (inode.bi_snapshot == snapshot)
		return 0;

	inode.bi_snapshot = snapshot;
	return __bch2_fsck_write_inode(trans, &inode);
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

	if (state != SNAPSHOT_ID_deleted &&
	    state != SNAPSHOT_ID_no_keys &&
	    state != SNAPSHOT_ID_empty)
		return 0;

	/*
	 * The incomplete snapshot deletion that stranded this key almost
	 * certainly stranded sibling keys across the content btrees too - left
	 * alone they only surface later, when copygc/reconcile trips over them.
	 * Schedule the content passes so the whole cascade of damage gets
	 * repaired in this fsck run rather than festering. require_recovery_pass
	 * only returns the unwind error (deferring this key's own repair) when
	 * it actually needs to rewind to an earlier pass; scheduling a
	 * later-or-current pass just marks it to run.
	 *
	 * skip_if_complete: at most one sweep per instance. These passes don't
	 * repair every stranded-key state - if they already ran and this key is
	 * still here, rescheduling can't fix it, but it would re-arm the passes
	 * in the superblock on every encounter, forcing fsck on every mount.
	 */
	ret = bch2_run_explicit_recovery_pass(c, &buf, BCH_RECOVERY_PASS_check_inodes,
					      RUN_RECOVERY_PASS_skip_if_complete) ?: ret;
	ret = bch2_run_explicit_recovery_pass(c, &buf, BCH_RECOVERY_PASS_check_extents,
					      RUN_RECOVERY_PASS_skip_if_complete) ?: ret;
	ret = bch2_run_explicit_recovery_pass(c, &buf, BCH_RECOVERY_PASS_check_dirents,
					      RUN_RECOVERY_PASS_skip_if_complete) ?: ret;
	ret = bch2_run_explicit_recovery_pass(c, &buf, BCH_RECOVERY_PASS_check_xattrs,
					      RUN_RECOVERY_PASS_skip_if_complete) ?: ret;

	/*
	 * Snapshot missing entirely: we should have caught this with
	 * btree_lost_data and kicked off reconstruct_snapshots, so if we end up
	 * here we have no idea what happened - force reconstruct too.
	 */
	if (state == SNAPSHOT_ID_empty &&
	    c->sb.btrees_lost_data & BIT_ULL(BTREE_ID_snapshots))
		ret = bch2_require_recovery_pass(c, &buf, BCH_RECOVERY_PASS_reconstruct_snapshots) ?: ret;

	/*
	 * Both repairs below destroy or relocate a key based on the in-memory
	 * snapshot table. Only trust it if the snapshots and subvolumes btrees
	 * have both been validated consistent (by check_snapshots /
	 * check_subvols) and not mutated since - the btrees_clean bits. If they
	 * haven't, the table may simply be stale and acting on it would destroy
	 * live data; schedule the passes and defer instead. Unlike
	 * require_recovery_pass on its own, this doesn't trust a pass that merely
	 * ran this mount (or was ratelimited) - it must have run since the last
	 * mutation.
	 */
	if (!bch2_btree_is_clean(c, BTREE_ID_snapshots) ||
	    !bch2_btree_is_clean(c, BTREE_ID_subvolumes)) {
		ret = bch2_require_recovery_pass(c, &buf, BCH_RECOVERY_PASS_check_snapshots) ?: ret;
		ret = bch2_require_recovery_pass(c, &buf, BCH_RECOVERY_PASS_check_subvols) ?: ret;
	}

	try(ret);

	/* inodes btree keys the inum in the offset field, everything else in inode */
	u64 inum = iter->btree_id == BTREE_ID_inodes ? k.k->p.offset : k.k->p.inode;
	unsigned repair_flags = FSCK_CAN_IGNORE | (!ret ? FSCK_CAN_FIX : 0);

	if (state == SNAPSHOT_ID_deleted ||
	    state == SNAPSHOT_ID_no_keys) {
		/*
		 * If there's no live descendant (a leaf, or an interior node whose
		 * subtree is entirely deleted) the key is genuinely orphaned -
		 * nothing can see it - so delete it. If there is a live descendant
		 * the key is still visible to it via inheritance and should have
		 * been migrated there during deletion; migrate it now rather than
		 * dropping it (the deleted node retains a child pointer so
		 * bch2_snapshot_live_descendent() can find the target).
		 */
		u32 live_child;
		int r = bch2_snapshot_live_descendent(c, k.k->p.snapshot, &live_child);
		if (r) {
			/*
			 * Dangling child pointer, on a table check_snapshots just
			 * validated clean (we only reach here clean) - a real
			 * inconsistency, not a stale table. Surface it; don't destroy
			 * the key.
			 */
			ret = r;
			goto fsck_err;
		}

		if (__fsck_err_on(!live_child,
				trans, repair_flags, bkey_in_deleted_snapshot,
				"key in deleted snapshot %s, delete?",
				(bch2_btree_id_to_text(&buf, iter->btree_id),
				 prt_char(&buf, ' '),
				 bch2_bkey_val_to_text(&buf, c, k), buf.buf))) {
			bch2_fsck_damaged(trans, SPOS(inum, 0, k.k->p.snapshot),
					  FSCK_DAMAGE_keys_deleted);
			ret = bch2_btree_delete_at(trans, iter,
						   BTREE_UPDATE_internal_snapshot_node) ?: 1;
		}

		if (__fsck_err_on(live_child,
				trans, repair_flags, bkey_in_deleted_interior_snapshot,
				"key in deleted interior snapshot %s, migrating to live descendant %u",
				(bch2_btree_id_to_text(&buf, iter->btree_id),
				 prt_char(&buf, ' '),
				 bch2_bkey_val_to_text(&buf, c, k), buf.buf), live_child))
			ret = bch2_delete_dead_snapshot_key(trans, iter, k, live_child) ?:
			      check_key_has_inode_in_snapshot(trans, iter->btree_id,
							      k.k->p.inode, live_child) ?:
			      1;
	} else {
		if (__fsck_err(trans, repair_flags, bkey_in_missing_snapshot,
			     "key in missing snapshot %s, delete?",
			     (bch2_btree_id_to_text(&buf, iter->btree_id),
			      prt_char(&buf, ' '),
			      bch2_bkey_val_to_text(&buf, c, k), buf.buf))) {
			bch2_fsck_damaged(trans, SPOS(inum, 0, k.k->p.snapshot),
					  FSCK_DAMAGE_keys_deleted);
			ret = bch2_btree_delete_at(trans, iter,
						   BTREE_UPDATE_internal_snapshot_node) ?: 1;
		}
	}
fsck_err:
	return ret;
}
