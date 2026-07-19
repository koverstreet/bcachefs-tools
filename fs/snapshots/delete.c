// SPDX-License-Identifier: GPL-2.0
#include "bcachefs.h"

#include "alloc/accounting.h"
#include "alloc/buckets.h"

#include "btree/bbpos.h"
#include "btree/update.h"

#include "init/error.h"
#include "init/progress.h"
#include "init/passes.h"

#include "snapshots/snapshot.h"
#include "snapshots/subvolume.h"

#include "util/enumerated_ref.h"

#include <linux/random.h>

/*
 * Snapshot trees:
 *
 * A node in a snapshot tree references keys with that snapshot ID, and all keys
 * with ancestor snapshot IDs not overwritten by a descendent snapshot.
 *
 * When a subvolume is deleted, we now have dead and redundant snapshot nodes
 * that must be cleaned up.
 *
 * - Dead:
 *
 *   A snapshot node with no children, and without a subvolume pointing to it,
 *   is unreferenced and can be deleted
 *
 * - Redundant:
 *
 *   Interior snapshot nodes (nodes with children) are only referenced by their
 *   child snapshot nodes. An interior node with only one child is redundant; we
 *   can clean it up by moving all non-overwritten keys to the child snapshot
 *   and removing it from the snapshot tree.
 *
 * Snapshot node states:
 *
 * - WILL_DELETE: this doesn't need to be a separate state bit. Indicates a leaf
 *   node that's no longer referenced by a subvolume (bch_snapshot.subvol == 0),
 *   so it's pending deletion
 *
 * - NO_KEYS: We can't remove interior nodes from the snapshot tree at runtime,
 *   because that can require changing bch_snapshot.depth on arbitrarily many
 *   children, and we can't do that atomically.
 *
 *   So instead, at runtime we'll do the heavy lifting of removing all keys that
 *   reference that snapshot ID, leave it in a half dead state, and the next
 *   time we start up we'll remove it from the snapshot tree.
 *
 *   Technically, we could, because the codepaths where this matters use the
 *   RCU-protected snapshot table - but there's a lot of work that has to be
 *   done for deleting interior snapshot nodes; parent/child pointers need to be
 *   updated, skiplists need to be adjusted, and if we get any of this wrong
 *   things can and will go horrifically wrong.
 *
 *   But if we defer it until recovery, when we're not yet running multithreaded,
 *   we can also run the check_snapshots recovery pass afterwards, for extra
 *   safety.
 */

/*
 * Recovery ordering: on a filesystem that may be damaged, delete_dead_snapshots()
 * must run AFTER the content-check passes - check_inodes, check_extents,
 * check_dirents, check_xattrs.
 *
 * Deleting a dead snapshot migrates its still-live keys down to a live
 * descendant, and that migration trusts the tree it's handed: it assumes every
 * key's inode exists at the key's snapshot ID and that the
 * inode<->dirent<->extent relationships are already consistent. When that
 * doesn't hold, migrating against the broken structure compounds the damage -
 * a dirent gets moved to a descendant whose inode is gone, or one half of an
 * inode<->dirent pair is deleted while the other is left stranded in a snapshot
 * the worklist won't revisit.
 *
 * The content-check passes repair exactly those properties, so running them
 * first hands us a consistent tree. The fingerprint of getting it wrong is
 * bidirectional, snapshot-local inode<->dirent breakage: dirents pointing to
 * missing inodes in one snapshot, inodes with no dirent in another.
 */

static __cold void bch2_snapshot_delete_nodes_to_text(struct printbuf *out, struct snapshot_delete *d, bool full)
{
	size_t limit = !full ? 10 : SIZE_MAX;

	prt_printf(out, "deleting from trees");
	darray_for_each_max(d->deleting_from_trees, i, limit)
		prt_printf(out, " %u", *i);

	if (d->deleting_from_trees.nr > limit)
		prt_str(out, " (many)");
	prt_newline(out);

	prt_printf(out, "deleting leaves");
	darray_for_each_max(d->delete_leaves, i, limit)
		prt_printf(out, " %u", *i);

	if (d->delete_leaves.nr > limit)
		prt_str(out, " (many)");
	prt_newline(out);

	prt_printf(out, "interior");
	darray_for_each_max(d->delete_interior, i, limit)
		prt_printf(out, " %u->%u", i->id, i->live_child);

	if (d->delete_interior.nr > limit)
		prt_str(out, " (many)");
	prt_newline(out);
}

__cold void bch2_snapshot_delete_status_to_text(struct printbuf *out, struct bch_fs *c)
{
	struct snapshot_delete *d = &c->snapshots.delete;

	if (!d->running) {
		prt_str(out, "(not running)");
		return;
	}

	scoped_guard(mutex, &d->progress_lock) {
		prt_printf(out, "Snapshot deletion v%u\n", d->version);
		prt_str(out, "Progress: ");
		bch2_progress_to_text(out, &d->progress);
		prt_newline(out);
		bch2_snapshot_delete_nodes_to_text(out, d, false);
	}
}

/*
 * Mark a snapshot as deleted, for future cleanup:
 */
int bch2_snapshot_node_set_deleted(struct btree_trans *trans, u32 id)
{
	struct bkey_i_snapshot *s =
		bch2_bkey_get_mut_typed(trans, BTREE_ID_snapshots, POS(0, id), 0, snapshot);
	int ret = PTR_ERR_OR_ZERO(s);
	bch2_fs_inconsistent_on(bch2_err_matches(ret, ENOENT), trans->c, "missing snapshot %u", id);
	if (unlikely(ret))
		return ret;

	/* already deleted? */
	if (bch2_snapshot_state(&s->v) != SNAPSHOT_STATE_live)
		return 0;

	/*
	 * The backref is retained: it now points at the subvolume's
	 * tombstone, and deletion requires that testimony - a will_delete
	 * leaf without a subvolume pointing back is an invalid state
	 * (check_should_delete_leaf):
	 */
	bch2_snapshot_state_set(&s->v, SNAPSHOT_STATE_will_delete);
	return 0;
}

/*
 * Sanity check before a destructive snapshot-node transition (emptying or
 * deleting a node): the per-snapshot disk accounting counters must be zero.
 *
 * The deletion scan should already have migrated or removed every key stamped
 * with this snapshot id; this verifies it did. A nonzero count means a key is
 * still accounted to the node, and one of two things is wrong:
 *
 *  - the accounting is stale/incorrect, or
 *  - the inodes btree is missing an entry: the deletion scan relies on "an
 *    extent/dirent/xattr in snapshot X implies an inode in snapshot X" to find
 *    the keys to remove, so a missing inode strands that snapshot's keys.
 *
 * Refuse the transition and schedule check_allocations (recompute accounting)
 * and check_inodes (revalidate the inode<->snapshot mapping) to resolve which,
 * rather than dropping the keys.
 *
 * The key count catches metadata-only stranding (dirents, xattrs, empty
 * inodes) that the sectors counter can't see. It's only trusted once
 * check_allocations has rebuilt it (scheduled by the per_dev_fragmentation_lru
 * upgrade); before that version we fall back to the sectors-only check. Either
 * way it's an in-memory read per snapshot btree, and the per-btree breakdown
 * points at where any stranded keys live.
 */
static int bch2_snapshot_node_check_no_data(struct btree_trans *trans, u32 id)
{
	struct bch_fs *c = trans->c;

	bool trust_keys = c->sb.version_upgrade_complete >=
		bcachefs_metadata_version_per_dev_fragmentation_lru;

	CLASS(printbuf, buf)();
	u64 total_keys = 0, total_sectors = 0;

	for (unsigned btree = 0; btree < BTREE_ID_NR; btree++) {
		if (!btree_type_has_snapshots(btree))
			continue;

		struct disk_accounting_pos acc;
		memset(&acc, 0, sizeof(acc));
		acc.type = BCH_DISK_ACCOUNTING_snapshot;
		acc.snapshot.id = id;
		acc.snapshot.btree = btree;

		u64 v[3] = {};
		bch2_accounting_mem_read(c, disk_accounting_pos_to_bpos(&acc), v, ARRAY_SIZE(v));

		u64 nr_keys	= trust_keys ? v[0] : 0;
		u64 key_bytes	= trust_keys ? v[1] : 0;
		u64 sectors	= v[2];

		if (!nr_keys && !sectors)
			continue;

		total_keys	+= nr_keys;
		total_sectors	+= sectors;

		prt_str(&buf, "\n  ");
		bch2_btree_id_to_text(&buf, btree);
		prt_printf(&buf, ": %llu keys (%llu bytes), %llu sectors",
			   nr_keys, key_bytes, sectors);
	}

	if (likely(!total_keys && !total_sectors))
		return 0;

	CLASS(printbuf, msg)();
	prt_printf(&msg, "snapshot node %u still has %llu keys / %llu sectors accounted to it - refusing to delete/empty, to prevent data loss; scheduling repair:%s\n",
		   id, total_keys, total_sectors, buf.buf);

	int ret = bch2_require_recovery_pass(c, &msg, BCH_RECOVERY_PASS_check_allocations);
	ret = bch2_require_recovery_pass(c, &msg, BCH_RECOVERY_PASS_check_inodes) ?: ret;

	bch_err(c, "%s", msg.buf);

	return ret ?: bch_err_throw(c, EINVAL_snapshot_delete_with_data);
}

static int bch2_snapshot_node_set_no_keys(struct btree_trans *trans, u32 id)
{
	struct bkey_i_snapshot *s =
		bch2_bkey_get_mut_typed(trans, BTREE_ID_snapshots, POS(0, id), 0, snapshot);
	int ret = PTR_ERR_OR_ZERO(s);
	bch2_fs_inconsistent_on(bch2_err_matches(ret, ENOENT), trans->c, "missing snapshot %u", id);
	if (unlikely(ret))
		return ret;

	try(bch2_snapshot_node_check_no_data(trans, id));

	bch2_snapshot_state_set(&s->v, SNAPSHOT_STATE_no_keys);
	return 0;
}

static inline void normalize_snapshot_child_pointers(struct bch_snapshot *s)
{
	if (le32_to_cpu(s->children[0]) < le32_to_cpu(s->children[1]))
		swap(s->children[0], s->children[1]);
}

static int bch2_snapshot_node_delete(struct btree_trans *trans, u32 id, bool delete_interior)
{
	struct bch_fs *c = trans->c;

	struct bkey_i_snapshot *s =
		bch2_bkey_get_mut_typed(trans, BTREE_ID_snapshots, POS(0, id), 0, snapshot);
	int ret = PTR_ERR_OR_ZERO(s);
	bch2_fs_inconsistent_on(bch2_err_matches(ret, ENOENT), c,
				"missing snapshot %u", id);

	if (ret)
		return ret;

	try(bch2_snapshot_node_check_no_data(trans, id));

	if (bch2_trans_inconsistent_on(bch2_snapshot_state(&s->v) == SNAPSHOT_STATE_deleted, trans,
			"deleting snapshot node %u: already in state deleted", id))
		return bch_err_throw(c, EINVAL_snapshot_delete_already_deleted);

	if (s->v.children[1]) {
		CLASS(bch_log_msg, msg)(c);
		prt_printf(&msg.m, "deleting node with two children:\n");
		bch2_snapshot_tree_keys_to_text(&msg.m, trans, id);
		bch2_snapshot_delete_nodes_to_text(&msg.m, &c->snapshots.delete, true);
		return bch_err_throw(c, EINVAL_snapshot_delete_has_two_children);
	}

	if (s->v.subvol) {
		/* deletion path: see the deleted tombstone directly, as in
		 * check_should_delete_leaf() - bch2_subvolume_get() would report
		 * it as ENOENT_subvolume_deleted */
		struct bch_subvolume subvol;
		try(bch2_bkey_get_val_typed(trans, BTREE_ID_subvolumes,
					    POS(0, le32_to_cpu(s->v.subvol)),
					    BTREE_ITER_cached, subvolume, &subvol));

		if (s->v.children[0] ||
		    (bch2_subvolume_state(&subvol) != SUBVOLUME_STATE_deleted &&
		     c->sb.version_upgrade_complete >=
		     bcachefs_metadata_version_per_dev_fragmentation_lru)) {
			CLASS(bch_log_msg, msg)(c);
			prt_printf(&msg.m, "deleting node with bad subvolume pointer:\n");
			bch2_bkey_val_to_text(&msg.m, c, bkey_i_to_s_c(&s->k_i));
			return bch_err_throw(c, EINVAL_snapshot_delete_bad_subvol);
		}

		try(bch2_btree_delete(trans, BTREE_ID_subvolumes, POS(0, le32_to_cpu(s->v.subvol)), 0));
	}

	u32 parent_id = le32_to_cpu(s->v.parent);
	u32 child_id = le32_to_cpu(s->v.children[0]);

	if (parent_id) {
		struct bkey_i_snapshot *parent =
			bch2_bkey_get_mut_typed(trans, BTREE_ID_snapshots, POS(0, parent_id),
						0, snapshot);
		ret = PTR_ERR_OR_ZERO(parent);
		bch2_fs_inconsistent_on(bch2_err_matches(ret, ENOENT), c,
					"missing snapshot %u", parent_id);
		if (unlikely(ret))
			return ret;

		/* find entry in parent->children for node being deleted */
		unsigned i;
		for (i = 0; i < 2; i++)
			if (le32_to_cpu(parent->v.children[i]) == id)
				break;

		if (bch2_fs_inconsistent_on(i == 2, c,
					"snapshot %u missing child pointer to %u",
					parent_id, id))
			return bch_err_throw(c, ENOENT_snapshot);

		parent->v.children[i] = cpu_to_le32(child_id);

		normalize_snapshot_child_pointers(&parent->v);
	}

	if (child_id) {
		if (!delete_interior) {
			CLASS(bch_log_msg, msg)(c);
			prt_printf(&msg.m, "deleting interior node %llu with child %u at runtime:\n",
				   s->k.p.offset, child_id);
			bch2_snapshot_tree_keys_to_text(&msg.m, trans, id);
			bch2_snapshot_delete_nodes_to_text(&msg.m, &c->snapshots.delete, true);
			return bch_err_throw(c, EINVAL_snapshot_delete_interior_at_runtime);
		}

		struct bkey_i_snapshot *child =
			bch2_bkey_get_mut_typed(trans, BTREE_ID_snapshots, POS(0, child_id),
						0, snapshot);
		ret = PTR_ERR_OR_ZERO(child);
		bch2_fs_inconsistent_on(bch2_err_matches(ret, ENOENT), c,
					"missing snapshot %u", child_id);
		if (unlikely(ret))
			return ret;

		child->v.parent = cpu_to_le32(parent_id);
	}

	if (!parent_id) {
		/*
		 * We're deleting the root of a snapshot tree: update the
		 * snapshot_tree entry to point to the new root, or delete it if
		 * this is the last snapshot ID in this tree:
		 */
		struct bkey_i_snapshot_tree *s_t = errptr_try(bch2_bkey_get_mut_typed(trans,
				BTREE_ID_snapshot_trees, POS(0, le32_to_cpu(s->v.tree)),
				0, snapshot_tree));

		if (s->v.children[0]) {
			s_t->v.root_snapshot = s->v.children[0];
		} else {
			s_t->k.type = KEY_TYPE_deleted;
			set_bkey_val_u64s(&s_t->k, 0);
		}
	}

	if (!bch2_request_incompat_feature(c, bcachefs_metadata_version_snapshot_deletion_v2)) {
		s->v.parent		= 0;
		/*
		 * Retain the pointer to our live descendant: the node is spliced
		 * out of the live tree, but a stray key later found in this
		 * deleted snapshot must still be migrated to where it's visible,
		 * and bch2_snapshot_live_descendent() walks children[0] to find
		 * it. (child_id is 0 for a leaf - nothing to retain.)
		 */
		s->v.children[0]	= cpu_to_le32(child_id);
		s->v.children[1]	= 0;
		s->v.subvol		= 0;
		s->v.tree		= 0;
		s->v.depth		= 0;
		s->v.skip[0]		= 0;
		s->v.skip[1]		= 0;
		s->v.skip[2]		= 0;
		bch2_snapshot_state_set(&s->v, SNAPSHOT_STATE_deleted);
	} else {
		s->k.type = KEY_TYPE_deleted;
		set_bkey_val_u64s(&s->k, 0);
	}

	/*
	 * Delete accounting - one key per snapshot btree. Note that designated
	 * initializers will not reliably cause a struct to be zeroed if it's a
	 * union:
	 */
	for (unsigned btree = 0; btree < BTREE_ID_NR; btree++) {
		if (!btree_type_has_snapshots(btree))
			continue;

		struct disk_accounting_pos acc;
		memset(&acc, 0, sizeof(acc));
		acc.type = BCH_DISK_ACCOUNTING_snapshot;
		acc.snapshot.id = id;
		acc.snapshot.btree = btree;

		try(bch2_btree_bit_mod_buffered(trans, BTREE_ID_accounting,
						disk_accounting_pos_to_bpos(&acc),
						false));
	}

	return 0;
}

/*
 * If we have an unlinked inode in an internal snapshot node, and the inode
 * really has been deleted in all child snapshots, how does this get cleaned up?
 *
 * first there is the problem of how keys that have been overwritten in all
 * child snapshots get deleted (unimplemented?), but inodes may perhaps be
 * special?
 *
 * also: unlinked inode in internal snapshot appears to not be getting deleted
 * correctly if inode doesn't exist in leaf snapshots
 *
 * solution:
 *
 * for a key in an interior snapshot node that needs work to be done that
 * requires it to be mutated: iterate over all descendent leaf nodes and copy
 * that key to snapshot leaf nodes, where we can mutate it
 */

static inline u32 interior_delete_has_id(interior_delete_list *l, u32 id)
{
	struct snapshot_interior_delete *i = darray_find_p(*l, i, i->id == id);
	return i ? i->live_child : 0;
}

static int snapshot_interior_delete_cmp(const void *_l, const void *_r)
{
	const struct snapshot_interior_delete *l = _l;
	const struct snapshot_interior_delete *r = _r;

	return cmp_int(l->id, r->id);
}

static const struct snapshot_interior_delete *snapshot_id_dying(struct snapshot_delete *d, unsigned id)
{
	struct snapshot_interior_delete search = { id };

	const struct snapshot_interior_delete *ret =
		darray_eytzinger1_find(d->eytzinger_delete_list, snapshot_interior_delete_cmp, &search);

	if (IS_ENABLED(CONFIG_BCACHEFS_DEBUG)) {
		if (!ret) {
			BUG_ON(snapshot_list_has_id(&d->delete_leaves, id));
			BUG_ON(interior_delete_has_id(&d->delete_interior, id));
		} else if (!ret->live_child) {
			BUG_ON(!snapshot_list_has_id(&d->delete_leaves, id));
		} else {
			BUG_ON(ret->live_child != interior_delete_has_id(&d->delete_interior, id));
		}
	}

	return ret;
}

/*
 * Remove a key from a dying/deleted snapshot node, migrating it to that node's
 * live descendant first when there is one (live_child != 0): the key is still
 * visible to the descendant via inheritance, so dropping it outright would lose
 * data. Only copy it down if the descendant doesn't already have its own key at
 * that position. With no live descendant (a leaf) the key is just deleted.
 *
 * Shared by the deletion pass (delete_dead_snapshots_process_key) and the fsck
 * repair (bch2_check_key_has_snapshot).
 */
int bch2_delete_dead_snapshot_key(struct btree_trans *trans, struct btree_iter *iter,
				  struct bkey_s_c k, u32 live_child)
{
	struct bch_fs *c = trans->c;

	if (live_child) {
		BUG_ON(!bch2_snapshot_exists(c, live_child));

		struct bpos dst = k.k->p;
		dst.snapshot = live_child;

		CLASS(btree_iter, dst_iter)(trans, iter->btree_id, dst,
					    BTREE_ITER_all_snapshots|BTREE_ITER_intent);
		struct bkey_s_c dst_k = bkey_try(bch2_btree_iter_peek_slot(&dst_iter));

		if (bkey_deleted(dst_k.k)) {
			struct bkey_i *new = errptr_try(bch2_bkey_make_mut_noupdate(trans, k));

			new->k.p = dst;
			try(bch2_trans_update(trans, &dst_iter, new,
					      BTREE_UPDATE_internal_snapshot_node));
		}
	}

	return bch2_btree_delete_at(trans, iter, BTREE_UPDATE_internal_snapshot_node);
}

static int delete_dead_snapshots_process_key(struct btree_trans *trans,
					     struct btree_iter *iter,
					     struct bkey_s_c k)
{
	struct bch_fs *c = trans->c;
	struct snapshot_delete *d = &c->snapshots.delete;

	int ret = bch2_check_key_has_snapshot(trans, iter, k);
	if (ret < 0)
		return ret;
	if (ret)
		return bch2_trans_commit_lazy(trans, NULL, NULL, BCH_TRANS_COMMIT_no_enospc);

	const struct snapshot_interior_delete *dying = snapshot_id_dying(d, k.k->p.snapshot);
	if (!dying)
		return 0;

	return bch2_delete_dead_snapshot_key(trans, iter, k, dying->live_child);
}

static bool skip_unrelated_snapshot_tree(struct btree_trans *trans, struct btree_iter *iter, u64 *prev_inum)
{
	struct bch_fs *c = trans->c;
	struct snapshot_delete *d = &c->snapshots.delete;

	u64 inum = iter->btree_id != BTREE_ID_inodes
		? iter->pos.inode
		: iter->pos.offset;

	if (*prev_inum == inum)
		return false;

	*prev_inum = inum;

	bool ret = !snapshot_list_has_id(&d->deleting_from_trees,
					 bch2_snapshot_tree(c, iter->pos.snapshot));
	if (unlikely(ret)) {
		struct bpos pos = iter->pos;
		pos.snapshot = 0;
		if (iter->btree_id != BTREE_ID_inodes)
			pos.offset = U64_MAX;
		bch2_btree_iter_set_pos(iter, bpos_nosnap_successor(pos));
	}

	return ret;
}

static int delete_dead_snapshot_keys_v1_btree(struct btree_trans *trans, enum btree_id btree)
{
	struct bch_fs *c = trans->c;
	struct snapshot_delete *d = &c->snapshots.delete;

	CLASS(disk_reservation, res)(c);
	u64 prev_inum = 0;

	try(for_each_btree_key_commit(trans, iter,
			btree, POS_MIN,
			BTREE_ITER_prefetch|BTREE_ITER_all_snapshots, k,
			&res.r, NULL, BCH_TRANS_COMMIT_no_enospc, ({
		bch2_progress_update_iter(trans, &d->progress, &iter);

		if (skip_unrelated_snapshot_tree(trans, &iter, &prev_inum))
			continue;

		bch2_disk_reservation_put(c, &res.r);
		delete_dead_snapshots_process_key(trans, &iter, k);
	})));

	return 0;
}

static int delete_dead_snapshot_keys_v1(struct btree_trans *trans)
{
	struct bch_fs *c = trans->c;
	struct snapshot_delete *d = &c->snapshots.delete;

	bch2_progress_init(&d->progress, __func__, c, btree_has_snapshots_mask, 0);
	d->progress.silent	= true;
	d->version		= 1;

	for (unsigned btree = 0; btree < BTREE_ID_NR; btree++)
		if (btree_type_has_snapshots(btree) && btree != BTREE_ID_inodes)
			try(delete_dead_snapshot_keys_v1_btree(trans, btree));

	/*
	 * fsck assumes that we'll process the inodes btree last:
	 */
	try(delete_dead_snapshot_keys_v1_btree(trans, BTREE_ID_inodes));

	return 0;
}

static int delete_dead_snapshot_keys_range(struct btree_trans *trans,
					   struct disk_reservation *res,
					   enum btree_id btree,
					   struct bpos start, struct bpos end)
{
	struct bch_fs *c = trans->c;

	return for_each_btree_key_max_commit(trans, iter,
			btree, start, end,
			BTREE_ITER_prefetch|BTREE_ITER_all_snapshots, k,
			res, NULL, BCH_TRANS_COMMIT_no_enospc, ({
		bch2_disk_reservation_put(c, res);
		delete_dead_snapshots_process_key(trans, &iter, k);
	}));
}

static int delete_dead_snapshot_keys_v2(struct btree_trans *trans)
{
	struct bch_fs *c = trans->c;
	struct snapshot_delete *d = &c->snapshots.delete;
	CLASS(disk_reservation, res)(c);
	u64 prev_inum = 0;

	bch2_progress_init(&d->progress, __func__, c, BIT_ULL(BTREE_ID_inodes), 0);
	d->progress.silent	= true;
	d->version		= 2;

	CLASS(btree_iter, iter)(trans, BTREE_ID_inodes, POS_MIN,
				BTREE_ITER_prefetch|BTREE_ITER_all_snapshots);

	/*
	 * First, delete extents/dirents/xattrs
	 *
	 * If an extent/dirent/xattr is present in a given snapshot ID an inode
	 * must also be present in that same snapshot ID, so we can use this to
	 * greatly accelerate scanning:
	 */

	while (1) {
		struct bkey_s_c k;
		try(lockrestart_do(trans,
				bkey_err(k = bch2_btree_iter_peek(&iter))));
		if (!k.k)
			break;

		bch2_progress_update_iter(trans, &d->progress, &iter);

		if (skip_unrelated_snapshot_tree(trans, &iter, &prev_inum))
			continue;

		if (snapshot_id_dying(d, k.k->p.snapshot)) {
			struct bpos start	= POS(k.k->p.offset, 0);
			struct bpos end		= POS(k.k->p.offset, U64_MAX);

			try(delete_dead_snapshot_keys_range(trans, &res.r, BTREE_ID_extents, start, end));
			try(delete_dead_snapshot_keys_range(trans, &res.r, BTREE_ID_dirents, start, end));
			try(delete_dead_snapshot_keys_range(trans, &res.r, BTREE_ID_xattrs, start, end));

			bch2_btree_iter_set_pos(&iter, POS(0, k.k->p.offset + 1));
		} else {
			bch2_btree_iter_advance(&iter);
		}
	}

	/* Then the inodes */

	prev_inum = 0;
	try(for_each_btree_key_commit(trans, iter,
			BTREE_ID_inodes, POS_MIN,
			BTREE_ITER_prefetch|BTREE_ITER_all_snapshots, k,
			&res.r, NULL, BCH_TRANS_COMMIT_no_enospc, ({
		if (skip_unrelated_snapshot_tree(trans, &iter, &prev_inum))
			continue;

		bch2_disk_reservation_put(c, &res.r);
		delete_dead_snapshots_process_key(trans, &iter, k);
	})));

	return 0;
}

static int check_should_delete_leaf(struct btree_trans *trans, struct bkey_s_c_snapshot s)
{
	struct bch_fs *c = trans->c;

	CLASS(printbuf, buf)();
	bch2_bkey_val_to_text(&buf, c, s.s_c);

	switch (bch2_snapshot_state(s.v)) {
	case SNAPSHOT_STATE_live:
		return 0;
	case SNAPSHOT_STATE_will_delete:
		if (!s.v->subvol) {
			/*
			 * A will_delete leaf with no subvolume backref is safe to
			 * delete once check_subvols has confirmed no live subvolume
			 * points at a non-live snapshot (it halts recovery if one
			 * does). Require that pass - it can run online - rather than
			 * scanning the subvolumes btree here, or trusting the
			 * filesystem version, which can't distinguish a legacy
			 * will_delete leaf from real corruption.
			 */
			try(bch2_require_recovery_pass(c, &buf, BCH_RECOVERY_PASS_check_subvols));
		} else {
			/*
			 * Raw lookup, not bch2_subvolume_get(): this is the
			 * deletion path, the one caller that must see the deleted
			 * tombstone rather than have it reported as ENOENT.
			 */
			struct bch_subvolume subvol;
			try(bch2_bkey_get_val_typed(trans, BTREE_ID_subvolumes,
						    POS(0, le32_to_cpu(s.v->subvol)),
						    BTREE_ITER_cached, subvolume, &subvol));

			if (bch2_fs_inconsistent_on(bch2_subvolume_state(&subvol) != SUBVOLUME_STATE_deleted,
						    c, "snapshot marked for deletion but subvolume not marked for deletion\n%s",
						    buf.buf))
				return 0;

			if (bch2_fs_inconsistent_on(le32_to_cpu(subvol.snapshot) != s.k->p.offset,
						    c, "snapshot marked for deletion but subvolume does not point back\n%s",
						    buf.buf))
				return 0;
		}

		return 1;
	case SNAPSHOT_STATE_no_keys:
		/*
		 * An emptied interior node whose children have all been
		 * deleted is normally reaped in the same pass that deletes
		 * its last child - each node deletion is its own commit, so a
		 * childless no_keys node is what a crash in between leaves.
		 * Shouldn't occur otherwise; handle it gracefully - it's
		 * empty by construction and node_delete re-verifies no-data:
		 */
		return ret_fsck_err(trans, snapshot_no_keys_childless,
				    "childless no_keys snapshot node, deleting:\n%s",
				    buf.buf);
	default: {
		bch2_fs_inconsistent(c, "snapshot leaf in invalid state\n%s", buf.buf);
		return 0;
	}
	}
}

/*
 * For a given snapshot, if it doesn't have a subvolume that points to it, and
 * it doesn't have child snapshot nodes - it's now redundant and we can mark it
 * as deleted.
 */
static int check_should_delete_snapshot(struct btree_trans *trans, struct bkey_s_c k)
{
	if (k.k->type != KEY_TYPE_snapshot)
		return 0;

	struct bch_fs *c = trans->c;
	struct bkey_s_c_snapshot s = bkey_s_c_to_snapshot(k);

	if (bch2_snapshot_state(s.v) == SNAPSHOT_STATE_deleted)
		return 0;

	if (!s.v->children[0]) {
		int ret = check_should_delete_leaf(trans, s);
		if (ret <= 0)
			return ret;
	}

	/*
	 * The leaf check above is this body's last restart point: everything
	 * below is in-memory table lookups and list pushes, so a transaction
	 * restart replays the body having collected nothing. The lists aren't
	 * transactional - keep restartable work above this line, or
	 * collection double-adds on replay. (Repairs invalidate collected
	 * state differently: the deletion path resets the lists wholesale
	 * and rescans.)
	 *
	 */
	struct snapshot_delete *d = &c->snapshots.delete;
	u32 live_child = 0, nr_live_children = 0;

	/*
	 * Collection is the only list writer, so reading needs no lock;
	 * progress_lock is for sysfs readers and taken only for updates:
	 */
	for (unsigned i = 0; i < 2; i++) {
		u32 id = le32_to_cpu(s.v->children[i]);
		if (id && !snapshot_list_has_id(&d->delete_leaves, id)) {
			nr_live_children++;

			live_child = interior_delete_has_id(&d->delete_interior, id) ?:
				interior_delete_has_id(&d->no_keys, id) ?:
				id;
		}
	}

	if (nr_live_children == 2)
		return 0;

	/*
	 * The resolved live child is about to license key migration and a
	 * splice: if it isn't in the table, is self-referential, or isn't a
	 * descendant, the topology is damaged - schedule repair and bail out
	 * of deletion, which runs again once check_snapshots has fixed it:
	 */
	if (live_child &&
	    (live_child == s.k->p.offset ||
	     !bch2_snapshot_exists(c, live_child) ||
	     !bch2_snapshot_is_ancestor(trans, live_child, s.k->p.offset))) {
		CLASS(bch_log_msg, msg)(c);

		prt_printf(&msg.m, "snapshot deletion found damaged topology (resolved live child %u):\n",
			   live_child);
		bch2_bkey_val_to_text(&msg.m, c, s.s_c);

		int ret = bch2_run_explicit_recovery_pass(c, &msg.m,
					BCH_RECOVERY_PASS_check_snapshots, 0);
		return ret ?: bch_err_throw(c, EINVAL_snapshot_delete_bad_topology);
	}

	scoped_guard(mutex, &d->progress_lock) {
		if (bch2_snapshot_state(s.v) != SNAPSHOT_STATE_no_keys)
			try(snapshot_list_add_nodup(c, &d->deleting_from_trees,
						    bch2_snapshot_tree(c, s.k->p.offset)));

		if (!nr_live_children) {
			try(snapshot_list_add(c, &d->delete_leaves, s.k->p.offset));
		} else {
			struct snapshot_interior_delete n = {
				.id		= s.k->p.offset,
				.live_child	= live_child,
			};

			/*
			 * We're not doing any processing for NO_KEYS snapshot
			 * nodes, but we still track them so that we can find
			 * the correct live_child when deleting parents, above:
			 */
			if (bch2_snapshot_state(s.v) != SNAPSHOT_STATE_no_keys)
				try(darray_push(&d->delete_interior, n));
			else
				try(darray_push(&d->no_keys, n));
		}
	}

	return 0;
}

static inline u32 bch2_snapshot_nth_parent_skip(struct bch_fs *c, u32 id, u32 n,
						interior_delete_list *skip)
{
	guard(rcu)();
	struct snapshot_table *t = rcu_dereference(c->snapshots.table);

	while (interior_delete_has_id(skip, id))
		id = __bch2_snapshot_parent(c, t, id);

	while (n--) {
		do {
			id = __bch2_snapshot_parent(c, t, id);
		} while (interior_delete_has_id(skip, id));
	}

	return id;
}

static int bch2_fix_child_of_deleted_snapshot(struct btree_trans *trans,
					      struct btree_iter *iter, struct bkey_s_c k,
					      interior_delete_list *deleted)
{
	struct bch_fs *c = trans->c;
	u32 nr_deleted_ancestors = 0;

	if (k.k->type != KEY_TYPE_snapshot)
		return 0;

	if (interior_delete_has_id(deleted, k.k->p.offset))
		return 0;

	struct bkey_i_snapshot *s =
		errptr_try(bch2_bkey_make_mut_noupdate_typed(trans, k, snapshot));

	darray_for_each(*deleted, i)
		nr_deleted_ancestors += bch2_snapshots_same_tree(c, s->k.p.offset, i->id) &&
		bch2_snapshot_is_ancestor(trans, s->k.p.offset, i->id);

	if (!nr_deleted_ancestors)
		return 0;

	le32_add_cpu(&s->v.depth, -nr_deleted_ancestors);

	if (!s->v.depth) {
		s->v.skip[0] = 0;
		s->v.skip[1] = 0;
		s->v.skip[2] = 0;
	} else {
		u32 depth = le32_to_cpu(s->v.depth);
		u32 parent = bch2_snapshot_parent(c, s->k.p.offset);

		for (unsigned j = 0; j < ARRAY_SIZE(s->v.skip); j++) {
			u32 id = le32_to_cpu(s->v.skip[j]);

			if (interior_delete_has_id(deleted, id)) {
				id = bch2_snapshot_nth_parent_skip(c,
							parent,
							depth > 1
							? get_random_u32_below(depth - 1)
							: 0,
							deleted);
				s->v.skip[j] = cpu_to_le32(id);
			}
		}

		bubble_sort(s->v.skip, ARRAY_SIZE(s->v.skip), cmp_le32);
	}

	return bch2_trans_update(trans, iter, &s->k_i, 0);
}

static int delete_dead_snapshots_locked(struct bch_fs *c)
{
	CLASS(btree_trans, trans)(c);

	/*
	 * For every snapshot node: If we have no live children and it's not
	 * pointed to by a subvolume, delete it:
	 */
	try(for_each_btree_key(trans, iter, BTREE_ID_snapshots, POS_MIN, 0, k,
		check_should_delete_snapshot(trans, k)));

	struct snapshot_delete *d = &c->snapshots.delete;
	if (!d->delete_leaves.nr && !d->delete_interior.nr)
		return 0;

	/*
	 * Eytzinger trees with 1-based indexing are faster than 0-based
	 * indexing due to better cacheline alignment:
	 */
	try(darray_push(&d->eytzinger_delete_list, ((struct snapshot_interior_delete) {})));
	darray_for_each(d->delete_interior, i)
		try(darray_push(&d->eytzinger_delete_list, *i));
	darray_for_each(d->delete_leaves, i)
		try(darray_push(&d->eytzinger_delete_list, ((struct snapshot_interior_delete) { *i })));
	darray_eytzinger1_sort(d->eytzinger_delete_list, snapshot_interior_delete_cmp);

	CLASS(printbuf, buf)();
	bch2_snapshot_delete_nodes_to_text(&buf, d, false);
	try(commit_do(trans, NULL, NULL, 0, bch2_trans_log_msg(trans, &buf)));

	try(!bch2_request_incompat_feature(c, bcachefs_metadata_version_snapshot_deletion_v2)
	    ? delete_dead_snapshot_keys_v2(trans)
	    : delete_dead_snapshot_keys_v1(trans));

	darray_for_each(d->delete_leaves, i)
		try(commit_do(trans, NULL, NULL, 0,
			bch2_snapshot_node_delete(trans, *i, false)));

	darray_for_each(d->delete_interior, i)
		try(commit_do(trans, NULL, NULL, 0,
			bch2_snapshot_node_set_no_keys(trans, i->id)));

	return 0;
}

/*
 * Serialization is recovery.run_lock, asserted below: the delete_dead_snapshots
 * pass .fn runs under it (the framework holds run_lock while running passes),
 * and the sysfs force-trigger takes it explicitly. So no separate lock is
 * needed, and passes never run this concurrently.
 */
int __bch2_delete_dead_snapshots(struct bch_fs *c)
{
	struct snapshot_delete *d = &c->snapshots.delete;

	lockdep_assert_held(&c->recovery.run_lock);

	d->running = true;
	d->progress.pos = BBPOS_MIN;

	int ret = delete_dead_snapshots_locked(c);

	scoped_guard(mutex, &d->progress_lock) {
		darray_exit(&d->deleting_from_trees);
		darray_exit(&d->no_keys);
		darray_exit(&d->delete_interior);
		darray_exit(&d->delete_leaves);
		darray_exit(&d->eytzinger_delete_list);
		d->running = false;
	}

	bch2_recovery_pass_set_no_ratelimit(c, BCH_RECOVERY_PASS_check_snapshots);

	return ret;
}

int bch2_delete_dead_snapshots(struct bch_fs *c)
{
	if (!c->opts.auto_snapshot_deletion)
		return 0;

	return __bch2_delete_dead_snapshots(c);
}

static int bch2_get_dead_interior_snapshots(struct btree_trans *trans, struct bkey_s_c k,
					    interior_delete_list *delete)
{
	if (k.k->type != KEY_TYPE_snapshot)
		return 0;

	struct bkey_s_c_snapshot s = bkey_s_c_to_snapshot(k);

	if (bch2_snapshot_state(s.v) == SNAPSHOT_STATE_no_keys) {
		u32 live_child = 0, nr_live_children = 0;
		for (unsigned i = 0; i < 2; i++) {
			u32 id = le32_to_cpu(s.v->children[i]);
			if (id) {
				nr_live_children++;
				live_child = interior_delete_has_id(delete, id) ?: id;
			}
		}

		if (nr_live_children != 1)
			return 0;

		struct snapshot_interior_delete n = {
			.id		= k.k->p.offset,
			.live_child	= live_child,
		};

		return darray_push(delete, n);
	}

	return 0;
}

int bch2_delete_dead_interior_snapshots(struct bch_fs *c)
{
	CLASS(btree_trans, trans)(c);
	CLASS(interior_delete_list, delete)();

	try(for_each_btree_key(trans, iter, BTREE_ID_snapshots, POS_MIN, 0, k,
			       bch2_get_dead_interior_snapshots(trans, k, &delete)));

	if (delete.nr) {
		{
			CLASS(bch_log_msg_level, msg)(c, LOGLEVEL_notice);

			prt_printf(&msg.m, "Deleting interior snapshot nodes forces check_snapshots:\n");
			try(bch2_run_explicit_recovery_pass(c, &msg.m,
					BCH_RECOVERY_PASS_check_snapshots, 0));
		}

		try(bch2_check_snapshots_trans(trans));

		/*
		 * Fixing children of deleted snapshots can't be done completely
		 * atomically, if we crash between here and when we delete the interior
		 * nodes some depth fields will be off:
		 */
		try(for_each_btree_key_commit(trans, iter, BTREE_ID_snapshots, POS_MIN,
					      BTREE_ITER_intent, k,
					      NULL, NULL, BCH_TRANS_COMMIT_no_enospc,
			bch2_fix_child_of_deleted_snapshot(trans, &iter, k, &delete)));

		darray_for_each(delete, i) {
			int ret = commit_do(trans, NULL, NULL, 0,
				bch2_snapshot_node_delete(trans, i->id, true));
			if (!bch2_err_matches(ret, EROFS))
				bch_err_msg(c, ret, "deleting snapshot %u", i->id);
			if (ret)
				return ret;
		}
	}

	return 0;
}

static bool interior_snapshot_needs_delete(const struct bch_snapshot *s)
{
	/* If there's one child, it's redundant and keys will be moved to the child */
	return !!s->children[0] + !!s->children[1] == 1;
}

int bch2_check_snapshot_needs_deletion(struct btree_trans *trans, struct bkey_s_c k,
				       u32 *nr_empty_interior)
{
	if (k.k->type != KEY_TYPE_snapshot)
		return 0;

	struct bch_fs *c = trans->c;
	struct bch_snapshot s;
	bkey_val_copy_pad(&s, bkey_s_c_to_snapshot(k));
	enum bch_snapshot_state state = bch2_snapshot_state_compat(&s);

	if (state == SNAPSHOT_STATE_deleted)
		return 0;

	if (state == SNAPSHOT_STATE_no_keys)
		*nr_empty_interior += 1;
	else if (state == SNAPSHOT_STATE_will_delete ||
		 interior_snapshot_needs_delete(&s)) {
		/*
		 * Schedule the deleter through the recovery-pass machinery,
		 * like bch2_mark_snapshot - this catches the interior
		 * single-child case, which mark_snapshot (will_delete only)
		 * doesn't. Ephemeral + best-effort: ignore the return.
		 */
		CLASS(printbuf, buf)();
		bch2_run_explicit_recovery_pass(c, &buf,
				BCH_RECOVERY_PASS_delete_dead_snapshots,
				RUN_RECOVERY_PASS_ephemeral);
	}

	return 0;
}
