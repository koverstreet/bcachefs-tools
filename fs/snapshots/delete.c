// SPDX-License-Identifier: GPL-2.0
#include "bcachefs.h"

#include "alloc/accounting.h"
#include "alloc/buckets.h"

#include "btree/bbpos.h"
#include "btree/update.h"

#include "init/error.h"
#include "init/damage.h"
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
	 * tombstone, and deletion checks it - a will_delete leaf without a
	 * subvolume pointing back is an invalid state
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
/*
 * Total keys/sectors accounted to snapshot @id across the snapshotted
 * btrees, with a per-btree breakdown appended to @breakdown if non-NULL.
 * (nr_keys counters exist only post-upgrade and read as zero before.)
 */
int bch2_snapshot_accounting_totals(struct bch_fs *c, u32 id,
				    u64 *total_keys, u64 *total_sectors,
				    u64 *btrees_with_keys,
				    struct printbuf *breakdown)
{
	bool trust_keys = c->sb.version_upgrade_complete >=
		bcachefs_metadata_version_per_dev_fragmentation_lru;

	*total_keys = *total_sectors = 0;

	for (unsigned btree = 0; btree < BTREE_ID_NR; btree++) {
		if (!btree_type_has_snapshots(btree))
			continue;

		struct disk_accounting_pos acc;
		memset(&acc, 0, sizeof(acc));
		acc.type = BCH_DISK_ACCOUNTING_snapshot;
		acc.snapshot.id = id;
		acc.snapshot.btree = btree;

		/*
		 * In-mem: bch2_accounting_is_mem() covers everything but
		 * BCH_DISK_ACCOUNTING_inum, so the counters are current as
		 * deltas are applied. No btree read, no transaction, and no
		 * write buffer flush to make the values trustworthy.
		 */
		u64 v[3] = {};
		bch2_accounting_mem_read(c, disk_accounting_pos_to_bpos(&acc), v, ARRAY_SIZE(v));

		u64 nr_keys	= trust_keys ? v[0] : 0;
		u64 key_bytes	= trust_keys ? v[1] : 0;
		u64 sectors	= v[2];

		if (!nr_keys && !sectors)
			continue;

		*total_keys	+= nr_keys;
		*total_sectors	+= sectors;
		if (btrees_with_keys)
			*btrees_with_keys |= BIT_ULL(btree);

		if (breakdown) {
			prt_str(breakdown, "\n  ");
			bch2_btree_id_to_text(breakdown, btree);
			prt_printf(breakdown, ": %llu keys (%llu bytes), %llu sectors",
				   nr_keys, key_bytes, sectors);
		}
	}

	return 0;
}

static inline u32 interior_delete_has_id(interior_delete_list *l, u32 id)
{
	struct snapshot_interior_delete *i = darray_find_p(*l, i, i->id == id);
	return i ? i->live_child : 0;
}

/*
 * @op is the operation delete_dead_snapshots was performing - set_no_keys,
 * leaf delete, or interior delete - which the node key does not tell you, and
 * which is the first thing you need: whether we were emptying a node whose
 * keys should already have migrated down, or splicing out an interior.
 *
 * The node is printed too, because operation and shape are independent and
 * their disagreement is the signal: an interior delete refused on a node that
 * still reads as a leaf means something quite different from a leaf delete
 * refused on a redundant interior whose migration was interrupted partway.
 */
/*
 * Keys stranded in @btrees: schedule the content pass for each, which is what
 * repairs the cause (a key with no inode at its own snapshot) and so lets the
 * next deletion find them.
 */
static int schedule_content_passes(struct bch_fs *c, struct printbuf *msg, u64 btrees)
{
	int ret = 0;

	for (unsigned btree = 0; btree < BTREE_ID_NR; btree++) {
		if (!(btrees & BIT_ULL(btree)))
			continue;

		enum bch_recovery_pass pass;
		switch (btree) {
		case BTREE_ID_extents:	pass = BCH_RECOVERY_PASS_check_extents; break;
		case BTREE_ID_inodes:	pass = BCH_RECOVERY_PASS_check_inodes;	break;
		case BTREE_ID_dirents:	pass = BCH_RECOVERY_PASS_check_dirents;	break;
		case BTREE_ID_xattrs:	pass = BCH_RECOVERY_PASS_check_xattrs;	break;
		default:		continue;
		}

		ret = bch2_run_explicit_recovery_pass(c, msg, pass, 0) ?: ret;
	}

	return ret;
}

static int bch2_snapshot_node_check_no_data(struct btree_trans *trans, u32 id,
					    const char *op)
{
	struct bch_fs *c = trans->c;

	CLASS(printbuf, buf)();
	u64 total_keys, total_sectors, btrees_with_keys = 0;

	try(bch2_snapshot_accounting_totals(c, id, &total_keys, &total_sectors,
					    &btrees_with_keys, &buf));

	if (likely(!total_keys && !total_sectors))
		return 0;

	/* Only the refusal below needs the node itself: */
	struct bkey_i_snapshot s;
	int ret = bch2_snapshot_lookup_key(trans, id, &s);
	bch2_fs_inconsistent_on(bch2_err_matches(ret, ENOENT), c, "missing snapshot %u", id);
	if (ret)
		return ret;

	/*
	 * Whether an interior has a single live child is what the deletion
	 * recorded when it built its lists, not something to re-derive from the
	 * snapshot table: live_child is the migration target the deletion
	 * actually used, so if the two ever disagreed the list is the one that
	 * describes what happened.
	 */
	struct snapshot_delete *d = &c->snapshots.delete;
	u32 live_child = interior_delete_has_id(&d->delete_interior, id) ?:
			 interior_delete_has_id(&d->no_keys, id);

	const char *shape = !s.v.children[0]
		? "leaf"
		: (live_child ? "redundant interior" : "interior");

	CLASS(printbuf, msg)();
	prt_printf(&msg, "%s snapshot node %u (%s) still has %llu keys / %llu sectors accounted to it - refusing, to prevent data loss; scheduling repair:%s\n  ",
		   op, id, shape, total_keys, total_sectors, buf.buf);
	bch2_bkey_val_to_text(&msg, c, bkey_i_to_s_c(&s.k_i));
	prt_newline(&msg);

	/*
	 * Schedule the passes whose key_has_snapshot repairs can actually
	 * remove the stranded keys - per btree, from the accounting breakdown.
	 * check_inodes alone can't repair a stranded dirent; scheduling only
	 * it left the refusal firing forever. Plus check_allocations, since
	 * here the count itself may be what's wrong:
	 */
	ret = bch2_run_explicit_recovery_pass(c, &msg, BCH_RECOVERY_PASS_check_allocations, 0);

	ret = schedule_content_passes(c, &msg, btrees_with_keys) ?: ret;

	bch_err(c, "%s", msg.buf);

	/*
	 * At runtime the passes can't rewind - but the requirement was
	 * persisted to the superblock before cannot_rewind_recovery was
	 * returned, so they run at next mount and the refusal below is the
	 * complete runtime response:
	 */
	if (bch2_err_matches(ret, BCH_ERR_cannot_rewind_recovery))
		ret = 0;

	return ret ?: bch_err_throw(c, EINVAL_snapshot_delete_with_data);
}

static int snapshot_node_data_to_text(struct printbuf *out, struct bch_fs *c,
				      u32 id, u32 live_child)
{
	CLASS(printbuf, breakdown)();
	u64 nr_keys, sectors;

	try(bch2_snapshot_accounting_totals(c, id, &nr_keys, &sectors, NULL, &breakdown));

	prt_printf(out, "\n  %s %u", live_child ? "interior" : "leaf", id);
	if (live_child)
		prt_printf(out, " -> %u", live_child);
	prt_printf(out, ": %llu keys, %llu sectors%s", nr_keys, sectors, breakdown.buf);
	return 0;
}

/*
 * Every node we're about to delete, and what it holds. Renders into @out for
 * the caller to log: this runs under lockrestart_do, so printing here would
 * hold btree locks across a printk of one line per node, and a restart
 * re-drive would append a second copy. Reset for that re-drive.
 */
static int bch2_snapshot_delete_data_to_text(struct printbuf *out,
					     struct bch_fs *c,
					     struct snapshot_delete *d)
{
	printbuf_reset(out);

	prt_printf(out, "snapshot deletion, data accounted per node:");

	darray_for_each(d->delete_leaves, i)
		try(snapshot_node_data_to_text(out, c, *i, 0));

	darray_for_each(d->delete_interior, i)
		try(snapshot_node_data_to_text(out, c, i->id, i->live_child));

	return 0;
}

/*
 * Inodes excluded: those are what we're about to delete, accounted until we do.
 *
 * Pre-per_dev_fragmentation_lru the key counters aren't populated yet, so this
 * sees sectors only - a dirents- or xattrs-only stranding is invisible there.
 */
static int snapshot_content_empty(struct bch_fs *c, u32 id,
				  struct printbuf *msg, u64 *bad_btrees)
{
	u64 nr_keys, sectors, btrees = 0;
	CLASS(printbuf, breakdown)();

	try(bch2_snapshot_accounting_totals(c, id, &nr_keys, &sectors,
					    &btrees, &breakdown));

	btrees &= ~BIT_ULL(BTREE_ID_inodes);
	if (!btrees)
		return 0;

	if (!*bad_btrees)
		prt_printf(msg, "content still accounted to dying snapshots - refusing to "
			   "delete their inode keys, which is how the deletion scan finds "
			   "these keys at all:");

	*bad_btrees |= btrees;
	prt_printf(msg, "\n  snapshot %u:%s", id, breakdown.buf);
	return 0;
}

/*
 * Which content btrees still have keys accounted to a dying snapshot, as a
 * mask. Nothing may remain in them by the time we delete the inode keys - see
 * the caller.
 */
static int dying_snapshots_content_btrees(struct bch_fs *c,
					  struct snapshot_delete *d,
					  struct printbuf *msg,
					  u64 *bad_btrees)
{
	darray_for_each(d->delete_leaves, i)
		try(snapshot_content_empty(c, *i, msg, bad_btrees));

	darray_for_each(d->delete_interior, i)
		try(snapshot_content_empty(c, i->id, msg, bad_btrees));

	return 0;
}

static int bch2_snapshot_node_set_no_keys(struct btree_trans *trans, u32 id)
{
	/* Before the get_mut below queues anything - see bch2_snapshot_node_check_deletable() */
	try(bch2_snapshot_node_check_no_data(trans, id, "set_no_keys"));

	struct bkey_i_snapshot *s =
		bch2_bkey_get_mut_typed(trans, BTREE_ID_snapshots, POS(0, id), 0, snapshot);
	int ret = PTR_ERR_OR_ZERO(s);
	bch2_fs_inconsistent_on(bch2_err_matches(ret, ENOENT), trans->c, "missing snapshot %u", id);
	if (unlikely(ret))
		return ret;

	bch2_snapshot_state_set(&s->v, SNAPSHOT_STATE_no_keys);
	return 0;
}

static inline void normalize_snapshot_child_pointers(struct bch_snapshot *s)
{
	if (le32_to_cpu(s->children[0]) < le32_to_cpu(s->children[1]))
		swap(s->children[0], s->children[1]);
}

/*
 * Every reason we decline to delete a snapshot node, decided without touching
 * anything.
 *
 * These have to be answered before the deletion starts, never partway through
 * it. bch2_bkey_get_mut_typed() queues its update the moment it's called, so a
 * refusal after the first one leaves the transaction holding a fragment of a
 * deletion, which the caller then drops on the floor.
 *
 * And callers commit work ahead of the deletion that assumes it will happen:
 * bch2_delete_dead_interior_snapshots() rewrites every child's depth and skip[]
 * for the tree shape the deletion is going to produce, and commits that, before
 * it deletes a single node. A node refused after that leaves the tree
 * describing a deletion that never happened - skip[] entries pointing above
 * parent, which validation rejects on the next write, taking the filesystem
 * read only.
 *
 * So this is separable from the deletion on purpose: a caller that is about to
 * commit to a set of nodes asks first, and drops the refused ones.
 */
static int bch2_snapshot_node_check_deletable(struct btree_trans *trans, u32 id,
					      bool delete_interior)
{
	struct bch_fs *c = trans->c;

	try(bch2_snapshot_node_check_no_data(trans, id,
			delete_interior ? "interior delete" : "leaf delete"));

	struct bkey_i_snapshot s;
	int ret = bch2_snapshot_lookup_key(trans, id, &s);
	bch2_fs_inconsistent_on(bch2_err_matches(ret, ENOENT), c,
				"missing snapshot %u", id);
	if (ret)
		return ret;

	if (bch2_trans_inconsistent_on(bch2_snapshot_state(&s.v) == SNAPSHOT_STATE_deleted, trans,
			"deleting snapshot node %u: already in state deleted", id))
		return bch_err_throw(c, EINVAL_snapshot_delete_already_deleted);

	if (s.v.children[1]) {
		CLASS(bch_log_msg, msg)(c);
		prt_printf(&msg.m, "deleting node with two children:\n");
		bch2_snapshot_tree_keys_to_text(&msg.m, trans, id);
		bch2_snapshot_delete_nodes_to_text(&msg.m, &c->snapshots.delete, true);
		return bch_err_throw(c, EINVAL_snapshot_delete_has_two_children);
	}

	if (s.v.subvol) {
		/* deletion path: see the deleted tombstone directly, as in
		 * check_should_delete_leaf() - bch2_subvolume_get() would report
		 * it as ENOENT_subvolume_deleted */
		struct bch_subvolume subvol;
		try(bch2_bkey_get_val_typed(trans, BTREE_ID_subvolumes,
					    POS(0, le32_to_cpu(s.v.subvol)),
					    BTREE_ITER_cached, subvolume, &subvol));

		if (s.v.children[0] ||
		    (bch2_subvolume_state(&subvol) != SUBVOLUME_STATE_deleted &&
		     c->sb.version_upgrade_complete >=
		     bcachefs_metadata_version_per_dev_fragmentation_lru)) {
			CLASS(bch_log_msg, msg)(c);
			prt_printf(&msg.m, "deleting node with bad subvolume pointer:\n");
			bch2_bkey_val_to_text(&msg.m, c, bkey_i_to_s_c(&s.k_i));
			return bch_err_throw(c, EINVAL_snapshot_delete_bad_subvol);
		}
	}

	if (s.v.children[0] && !delete_interior) {
		CLASS(bch_log_msg, msg)(c);
		prt_printf(&msg.m, "deleting interior node %u with child %u at runtime:\n",
			   id, le32_to_cpu(s.v.children[0]));
		bch2_snapshot_tree_keys_to_text(&msg.m, trans, id);
		bch2_snapshot_delete_nodes_to_text(&msg.m, &c->snapshots.delete, true);
		return bch_err_throw(c, EINVAL_snapshot_delete_interior_at_runtime);
	}

	return 0;
}

/*
 * Callers are responsible for bch2_snapshot_node_check_deletable() first: from
 * here on we only fail on errors that take the whole thing down, never on a
 * refusal.
 */
int bch2_snapshot_node_delete(struct btree_trans *trans, u32 id)
{
	struct bch_fs *c = trans->c;

	struct bkey_i_snapshot *s =
		bch2_bkey_get_mut_typed(trans, BTREE_ID_snapshots, POS(0, id), 0, snapshot);
	int ret = PTR_ERR_OR_ZERO(s);
	bch2_fs_inconsistent_on(bch2_err_matches(ret, ENOENT), c,
				"missing snapshot %u", id);

	if (ret)
		return ret;

	if (s->v.subvol)
		try(bch2_btree_delete(trans, BTREE_ID_subvolumes,
				      POS(0, le32_to_cpu(s->v.subvol)), 0));

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
		/*
		 * Retain parent/child pointers; don't destroy information if we
		 * have to repair:
		 */
		s->v.subvol		= 0;
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
	 * Nothing here touches this snapshot's accounting. Accounting is
	 * derived: the triggers on the keys we just deleted are what took the
	 * counters to zero. A counter still nonzero here is a bug to find, not
	 * a number to correct - and check_no_data() above has already refused
	 * the deletion if that's the case.
	 */
	return 0;
}

/*
 * Can we relink to this node? Only a deleted node has been spliced out of the
 * tree. no_keys is an emptied interior - kept in the tree until the next
 * remount precisely so ancestry still resolves - and carries child pointers
 * like any other node, so it is a perfectly ordinary neighbour to attach to.
 */
static bool snapshot_in_tree(const struct bch_snapshot *s)
{
	return bch2_snapshot_state_compat(s) != SNAPSHOT_STATE_deleted;
}

/*
 * Reinsert an undeleted node into the live tree: the inverse of the splice in
 * bch2_snapshot_node_delete(). The tombstone retained its pointers as history:
 * if the parent's child slot (or the snapshot_tree root, for a deleted root)
 * currently holds one of our retained children, that's where the splice took
 * us out - take the slot back and take the child back. If the slot holds us
 * already, the deletion never got that far and there's nothing to relink. If
 * the topology has moved on, or an old wiped tombstone retained nothing, the
 * node revives unlinked and the tree pointer checks handle it downstream.
 */
int bch2_snapshot_node_undelete(struct btree_trans *trans, struct bkey_i_snapshot *u)
{
	struct bch_fs *c = trans->c;
	u32 id = u->k.p.offset;
	u32 child_id = le32_to_cpu(u->v.children[0]);

	CLASS(bch_log_msg, msg)(c);

	bch2_bkey_val_to_text(&msg.m, c, bkey_i_to_s_c(&u->k_i));
	prt_newline(&msg.m);

	if (u->v.children[1]) {
		bch_err(c, "cannot undelete a node with two children");
		return bch_err_throw(c, fsck_repair_unimplemented);
	}

	if (!u->v.children[0]) {
		/*
		 * Childless: no splice to undo - our retained parent pointer is
		 * intact. Handle the simple case, parent still live: revive in
		 * place, and if our slot in the parent was cleared, the edge
		 * repairs complete it from our side. A dead parent would need
		 * undeleting too - unexpected, so fail rather than guess.
		 *
		 * A childless root has no parent to relink through and no
		 * children to take back: it is the entire tree, so setting it
		 * live is the whole repair. That is the shape of a filesystem
		 * that has never been snapshotted, where refusing means
		 * refusing to mount at all. Whether a snapshot_tree still
		 * points at it is the tree pointer checks' business, as for any
		 * node that revives unlinked.
		 */
		u32 parent_id = le32_to_cpu(u->v.parent);
		if (parent_id) {
			struct bkey_i_snapshot *parent =
				bch2_bkey_get_mut_typed(trans, BTREE_ID_snapshots,
							POS(0, parent_id), 0, snapshot);
			int ret = PTR_ERR_OR_ZERO(parent);
			if (bch2_err_matches(ret, ENOENT)) {
				prt_printf(&msg.m, "cannot undelete: parent no longer exists");
				return bch_err_throw(c, fsck_repair_unimplemented);
			}
			if (ret)
				return ret;

			if (!snapshot_in_tree(&parent->v)) {
				prt_printf(&msg.m, "cannot undelete: parent is itself deleted");
				return bch_err_throw(c, fsck_repair_unimplemented);
			}

			if (le32_to_cpu(parent->v.children[0]) != id &&
			    le32_to_cpu(parent->v.children[1]) != id) {
				if (parent->v.children[0] && parent->v.children[1]) {
					prt_printf(&msg.m, "cannot undelete: parent's child slots are full");
					return bch_err_throw(c, fsck_repair_unimplemented);
				}

				unsigned i = !parent->v.children[0] ? 0 : 1;
				parent->v.children[i] = cpu_to_le32(id);
				normalize_snapshot_child_pointers(&parent->v);
			}
		}

		bch2_snapshot_state_set(&u->v, SNAPSHOT_STATE_live);
		return 0;
	}

	struct bkey_i_snapshot *child =
		bch2_bkey_get_mut_typed(trans, BTREE_ID_snapshots, POS(0, child_id), 0, snapshot);
	int ret = PTR_ERR_OR_ZERO(child);
	if (bch2_err_matches(ret, ENOENT)) {
		bch_err(c, "cannot undelete: child no longer exists");
		return bch_err_throw(c, fsck_repair_unimplemented);
	}
	if (ret)
		return ret;

	prt_printf(&msg.m, "attaching to child  ");
	bch2_bkey_val_to_text(&msg.m, c, bkey_i_to_s_c(&child->k_i));
	prt_newline(&msg.m);

	if (!snapshot_in_tree(&child->v)) {
		prt_printf(&msg.m, "cannot undelete: child is itself deleted");
		return bch_err_throw(c, fsck_repair_unimplemented);
	}

	u32 parent_id = le32_to_cpu(child->v.parent);
	if (parent_id && parent_id <= id) {
		prt_printf(&msg.m, "cannot undelete: parent of child < node to undelete");
		return bch_err_throw(c, fsck_repair_unimplemented);
	}

	child->v.parent = cpu_to_le32(id);

	if (parent_id) {
		struct bkey_i_snapshot *parent =
			bch2_bkey_get_mut_typed(trans, BTREE_ID_snapshots,
						POS(0, parent_id), 0, snapshot);
		int ret = PTR_ERR_OR_ZERO(parent);
		if (ret && !bch2_err_matches(ret, ENOENT))
			return ret;

		if (ret) {
			prt_printf(&msg.m, "cannot undelete: parent no longer exists");
			return bch_err_throw(c, fsck_repair_unimplemented);
		}

		prt_printf(&msg.m, "attaching to parent ");
		bch2_bkey_val_to_text(&msg.m, c, bkey_i_to_s_c(&parent->k_i));
		prt_newline(&msg.m);

		if (!snapshot_in_tree(&parent->v)) {
			prt_printf(&msg.m, "cannot undelete: parent is itself deleted");
			return bch_err_throw(c, fsck_repair_unimplemented);
		}

		for (unsigned i = 0; i < 2; i++) {
			u32 p_child = le32_to_cpu(parent->v.children[i]);

			if (p_child == child_id) {
				parent->v.children[i] = cpu_to_le32(id);
				normalize_snapshot_child_pointers(&parent->v);
				break;
			}
		}
	} else if (child->v.tree) {
		struct bkey_i_snapshot_tree *s_t =
			bch2_bkey_get_mut_typed(trans, BTREE_ID_snapshot_trees,
						POS(0, le32_to_cpu(child->v.tree)),
						0, snapshot_tree);
		int ret = PTR_ERR_OR_ZERO(s_t);
		if (ret && !bch2_err_matches(ret, ENOENT))
			return ret;

		if (!ret && le32_to_cpu(s_t->v.root_snapshot) == child_id) {
			s_t->v.root_snapshot = cpu_to_le32(id);

			prt_printf(&msg.m, "updated tree ");
			bch2_bkey_val_to_text(&msg.m, c, bkey_i_to_s_c(&s_t->k_i));
			prt_newline(&msg.m);
		}
	}

	u->v.parent	= cpu_to_le32(parent_id);
	u->v.tree	= child->v.tree;
	bch2_snapshot_state_set(&u->v, SNAPSHOT_STATE_live);

	u->v.depth = cpu_to_le32(bch2_snapshot_depth(c, parent_id));
	for (unsigned j = 0; j < ARRAY_SIZE(u->v.skip); j++)
		u->v.skip[j] = cpu_to_le32(bch2_snapshot_skiplist_get(c, parent_id));
	bubble_sort(u->v.skip, ARRAY_SIZE(u->v.skip), cmp_le32);

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
		} else if (iter->btree_id == BTREE_ID_damage &&
			   k.k->type == KEY_TYPE_damage &&
			   dst_k.k->type == KEY_TYPE_damage) {
			/*
			 * Damage entries merge instead of dropping: the dying
			 * key can hold counts recorded after the descendant's
			 * key was created, so "descendant overwrote it" never
			 * applies:
			 */
			struct bkey_i *new = errptr_try(bch2_damage_keys_merge(trans, dst, dst_k, k));
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

static int delete_dead_snapshot_keys_v1_btree(struct btree_trans *trans, enum btree_id btree)
{
	struct bch_fs *c = trans->c;
	struct snapshot_delete *d = &c->snapshots.delete;

	CLASS(disk_reservation, res)(c);

	try(for_each_btree_key_commit(trans, iter,
			btree, POS_MIN,
			BTREE_ITER_prefetch|BTREE_ITER_all_snapshots, k,
			&res.r, NULL, BCH_TRANS_COMMIT_no_enospc, ({
		bch2_progress_update_iter(trans, &d->progress, &iter);

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

	bch2_progress_init(&d->progress, __func__, c, BIT_ULL(BTREE_ID_inodes), 0);
	d->progress.silent	= true;
	d->version		= 2;

	CLASS(btree_iter, iter)(trans, BTREE_ID_inodes, POS_MIN,
				BTREE_ITER_prefetch|BTREE_ITER_all_snapshots);

	/*
	 * First, delete extents/dirents/xattrs and damage keys
	 *
	 * If an extent/dirent/xattr is present in a given snapshot ID an inode
	 * must also be present in that same snapshot ID (and a damage key has
	 * an inode at its exact position - enforced by check_damage), so we
	 * can use this to greatly accelerate scanning:
	 */

	while (1) {
		struct bkey_s_c k;
		try(lockrestart_do(trans,
				bkey_err(k = bch2_btree_iter_peek(&iter))));
		if (!k.k)
			break;

		bch2_progress_update_iter(trans, &d->progress, &iter);

		if (snapshot_id_dying(d, k.k->p.snapshot)) {
			struct bpos start	= POS(k.k->p.offset, 0);
			struct bpos end		= POS(k.k->p.offset, U64_MAX);

			try(delete_dead_snapshot_keys_range(trans, &res.r, BTREE_ID_extents, start, end));
			try(delete_dead_snapshot_keys_range(trans, &res.r, BTREE_ID_dirents, start, end));
			try(delete_dead_snapshot_keys_range(trans, &res.r, BTREE_ID_xattrs, start, end));
			try(delete_dead_snapshot_keys_range(trans, &res.r, BTREE_ID_damage,
							    POS(0, k.k->p.offset),
							    SPOS(0, k.k->p.offset, U32_MAX)));

			bch2_btree_iter_set_pos(&iter, POS(0, k.k->p.offset + 1));
		} else {
			bch2_btree_iter_advance(&iter);
		}
	}

	/*
	 * The scan above located keys through the inodes btree, so deleting
	 * inode keys destroys the index it used: anything left in the other
	 * btrees at a dying snapshot becomes unfindable, by this pass and by
	 * every later one. Check while it's still repairable - the
	 * check_no_data licenses at the end of deletion only fire once the
	 * inodes are already gone.
	 */
	CLASS(printbuf, msg)();
	u64 bad_btrees = 0;
	try(dying_snapshots_content_btrees(c, d, &msg, &bad_btrees));

	if (unlikely(bad_btrees)) {
		/*
		 * Accounting is derived from the keys themselves, so it doesn't
		 * go through the index above and can see what that scan missed.
		 * The v1 scan doesn't use the index either - rescan with it, on
		 * the btrees accounting named and no others.
		 */
		prt_str(&msg, "\nfalling back to the v1 scan");
		bch2_print_str(c, KERN_NOTICE, msg.buf);

		/*
		 * Retarget the progress indicator: it was initialized for the
		 * inodes btree, which is not what we're about to walk. Nothing
		 * after this loop updates progress, so this is the last word,
		 * and version says the run degraded.
		 */
		bch2_progress_init(&d->progress, __func__, c, bad_btrees, 0);
		d->progress.silent	= true;
		d->version		= 1;

		for (unsigned btree = 0; btree < BTREE_ID_NR; btree++)
			if (bad_btrees & BIT_ULL(btree))
				try(delete_dead_snapshot_keys_v1_btree(trans, btree));

		printbuf_reset(&msg);
		bad_btrees = 0;
		try(dying_snapshots_content_btrees(c, d, &msg, &bad_btrees));
	}

	if (unlikely(bad_btrees)) {
		/*
		 * v1 found nothing and the keys are still accounted, so nothing
		 * is hiding behind a missing inode - the count is what's wrong.
		 * No content passes: their key_has_snapshot repairs exist to
		 * uphold the invariant v1 doesn't need. Refuse anyway - the
		 * inode keys below are the only way left to find anything that
		 * is in fact stranded.
		 */
		int ret = bch2_run_explicit_recovery_pass(c, &msg,
					BCH_RECOVERY_PASS_check_allocations, 0);

		bch_err(c, "%s", msg.buf);

		if (bch2_err_matches(ret, BCH_ERR_cannot_rewind_recovery))
			ret = 0;

		return ret ?: bch_err_throw(c, EINVAL_snapshot_delete_with_data);
	}

	/* Then the inodes */

	try(for_each_btree_key_commit(trans, iter,
			BTREE_ID_inodes, POS_MIN,
			BTREE_ITER_prefetch|BTREE_ITER_all_snapshots, k,
			&res.r, NULL, BCH_TRANS_COMMIT_no_enospc, ({
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
 * Sort one node: dead (no live descendant - keys are dropped) or redundant
 * (exactly one live child - keys migrate to live_child). Two live children:
 * nothing to do.
 *
 * The caller scans in ascending id order and a node's id is always below its
 * parent's, so children are sorted before parents. Both cases need that:
 *
 * - A node whose children are all already on delete_leaves is itself dead and
 *   joins them, so a dead subtree accumulates bottom-up. It's torn down in
 *   that same order later, each node childless by its turn because deleting a
 *   child splices this node's pointer to zero.
 *
 * - A child that's itself redundant already has its own live_child recorded,
 *   so the lookup below returns the terminal of the whole collapse chain
 *   rather than the next node down, and keys move there in one hop.
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
	 * The loop's per-key commit can restart after the pushes below and
	 * replay this body - the lists aren't transactional, so every push
	 * must be idempotent: nodup adds and has_id guards, or collection
	 * double-adds on replay. (Repairs invalidate collected state
	 * differently: the deletion path resets the lists wholesale and
	 * rescans.)
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
			try(snapshot_list_add_nodup(c, &d->delete_leaves, s.k->p.offset));
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
			if (bch2_snapshot_state(s.v) != SNAPSHOT_STATE_no_keys) {
				if (!interior_delete_has_id(&d->delete_interior, n.id))
					try(darray_push(&d->delete_interior, n));
			} else {
				if (!interior_delete_has_id(&d->no_keys, n.id))
					try(darray_push(&d->no_keys, n));
			}
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
	 * pointed to by a subvolume, delete it.
	 *
	 * The per-key commit is for the childless-no_keys fsck_err: its fix
	 * is out-of-band (the node is only collected here, deleted later),
	 * but the fsck_err queues its repair log entry on this trans - which
	 * would otherwise never commit, only be dropped at the next
	 * trans_begin (the iter.c dropped-updates WARN):
	 */
	try(for_each_btree_key_commit(trans, iter, BTREE_ID_snapshots, POS_MIN, 0, k,
		NULL, NULL, 0,
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

	/*
	 * What each node holds going in - the same counters the check_no_data
	 * licenses read on the way out, so whatever is still there at the end
	 * is what the migration didn't move.
	 */
	CLASS(printbuf, node_data)();
	try(bch2_snapshot_delete_data_to_text(&node_data, c, d));
	bch_info(c, "%s", node_data.buf);

	try(!bch2_request_incompat_feature(c, bcachefs_metadata_version_snapshot_deletion_v2)
	    ? delete_dead_snapshot_keys_v2(trans)
	    : delete_dead_snapshot_keys_v1(trans));

	/*
	 * Refusals first, and a refusal stops the whole deletion: nodes go one
	 * commit at a time, so refusing partway leaves the ones already deleted
	 * describing a tree shape the rest of the deletion no longer produces.
	 * The refusal schedules its own repair - deletion runs again after it.
	 *
	 * This can't precede the migration above: what the check reads is
	 * whether the keys that were supposed to move actually did.
	 */
	darray_for_each(d->delete_leaves, i)
		try(lockrestart_do(trans,
			bch2_snapshot_node_check_no_data(trans, *i, "leaf delete")));

	darray_for_each(d->delete_interior, i)
		try(lockrestart_do(trans,
			bch2_snapshot_node_check_no_data(trans, i->id, "set_no_keys")));

	darray_for_each(d->delete_leaves, i)
		try(commit_do(trans, NULL, NULL, 0,
			bch2_snapshot_node_check_deletable(trans, *i, false) ?:
			bch2_snapshot_node_delete(trans, *i)));

	darray_for_each(d->delete_interior, i)
		try(commit_do(trans, NULL, NULL, 0,
			bch2_snapshot_node_set_no_keys(trans, i->id)));

	return 0;
}

/*
 * A refusal means "repair first, then retry", not failure: the check that
 * refused has scheduled the passes that fix what it found, and deletion is
 * rescheduled the next time a dying snapshot is seen (bch2_mark_snapshot,
 * bch2_check_snapshot_needs_deletion). Nothing has been done, so there is
 * nothing wrong with the filesystem that going read-only would protect.
 *
 * These run as recovery passes, and bch2_run_recovery_passes() returns the
 * first pass error it sees - which at startup fails the mount. So the refusals
 * stop here.
 */
static bool snapshot_delete_refused(int ret)
{
	return ret == -BCH_ERR_EINVAL_snapshot_delete_with_data ||
	       ret == -BCH_ERR_EINVAL_snapshot_delete_bad_topology;
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
	if (snapshot_delete_refused(ret))
		ret = 0;

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

static int delete_dead_interior_snapshots(struct bch_fs *c)
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

		/*
		 * This is also what makes the non-atomic depth rewrite below
		 * safe to retry: check_snapshots re-derives every depth from
		 * the parent chain. If we crashed partway through that rewrite,
		 * the surviving children carry decremented depths while their
		 * parent pointers are still untouched - so this restores them,
		 * and the decrement lands exactly once per run.
		 */
		try(bch2_check_snapshots_trans(trans));

		/*
		 * check_snapshots is licensed to change node states (e.g.
		 * repairing a stale-state legacy tombstone to deleted), so the
		 * pre-check collection is only good for deciding whether to run
		 * it - re-collect for the authoritative delete list:
		 */
		delete.nr = 0;
		try(for_each_btree_key(trans, iter, BTREE_ID_snapshots, POS_MIN, 0, k,
				       bch2_get_dead_interior_snapshots(trans, k, &delete)));
	}

	if (delete.nr) {
		/*
		 * Refusals first: the fixup below commits every surviving child's
		 * new depth and skip[] for the tree shape these deletions produce,
		 * so a node refused after it leaves the children describing a
		 * deletion that never happened - skip[] pointing above parent,
		 * which validation rejects on the next write.
		 */
		darray_for_each(delete, i)
			try(lockrestart_do(trans,
				bch2_snapshot_node_check_deletable(trans, i->id, true)));

		/*
		 * Fixing children of deleted snapshots can't be done completely
		 * atomically, if we crash between here and when we delete the interior
		 * nodes some depth fields will be off - the check_snapshots above is
		 * what puts them back on the next run.
		 *
		 * So for the whole span between this loop and the delete loop
		 * below, a child carries its new depth and its old parent
		 * pointer: depth != parent's depth + 1 is a state this pass
		 * passes through, not an invariant.
		 */
		try(for_each_btree_key_commit(trans, iter, BTREE_ID_snapshots, POS_MIN,
					      BTREE_ITER_intent, k,
					      NULL, NULL, BCH_TRANS_COMMIT_no_enospc,
			bch2_fix_child_of_deleted_snapshot(trans, &iter, k, &delete)));

		darray_for_each(delete, i) {
			int ret = commit_do(trans, NULL, NULL, 0,
				bch2_snapshot_node_delete(trans, i->id));
			if (!bch2_err_matches(ret, EROFS))
				bch_err_msg(c, ret, "deleting snapshot %u", i->id);
			if (ret)
				return ret;
		}
	}

	return 0;
}

int bch2_delete_dead_interior_snapshots(struct bch_fs *c)
{
	if (!c->opts.auto_snapshot_deletion)
		return 0;

	int ret = delete_dead_interior_snapshots(c);
	return snapshot_delete_refused(ret) ? 0 : ret;
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
