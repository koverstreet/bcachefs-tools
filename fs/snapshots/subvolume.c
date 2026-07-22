// SPDX-License-Identifier: GPL-2.0

#include "bcachefs.h"

#include "btree/key_cache.h"
#include "btree/update.h"

#include "fs/namei.h"

#include "vfs/fs.h"

#include "init/error.h"
#include "init/passes.h"
#include "init/recovery.h"

#include "snapshots/snapshot.h"
#include "snapshots/subvolume.h"

#include "util/enumerated_ref.h"

#include <linux/random.h>

static int bch2_subvolume_set_deleted(struct btree_trans *, u32);

static int bch2_subvolume_missing(struct bch_fs *c, u32 subvolid)
{
	CLASS(bch_log_msg, msg)(c);

	prt_printf(&msg.m, "missing subvolume %u", subvolid);
	msg.m.suppress = !bch2_count_fsck_err(c, subvol_missing, &msg.m);

	return bch2_run_explicit_recovery_pass(c, &msg.m, BCH_RECOVERY_PASS_check_inodes, 0);
}

static struct bpos subvolume_children_pos(struct bkey_s_c k)
{
	if (k.k->type != KEY_TYPE_subvolume)
		return POS_MIN;

	struct bkey_s_c_subvolume s = bkey_s_c_to_subvolume(k);
	if (!s.v->fs_path_parent)
		return POS_MIN;
	return POS(le32_to_cpu(s.v->fs_path_parent), s.k->p.offset);
}

static int check_subvol(struct btree_trans *trans,
			struct btree_iter *iter,
			struct bkey_s_c k)
{
	struct bch_fs *c = trans->c;
	struct bch_subvolume subvol;
	struct bch_snapshot snapshot;
	CLASS(printbuf, buf)();
	unsigned snapid;
	int ret = 0;

	if (k.k->type != KEY_TYPE_subvolume)
		return 0;

	bkey_val_copy_pad(&subvol, bkey_s_c_to_subvolume(k));

	/*
	 * A zero state field means the key predates the state field, or was
	 * wiped: recover it from the legacy flag bits whenever it's unset,
	 * regardless of upgrade status (see check_snapshot). Silent mid-upgrade
	 * (the expected migration); post-upgrade it's unexpected, so surface it.
	 */
	if (!bch2_subvolume_state(&subvol)) {
		bool upgrading = c->sb.version_upgrade_complete <
			bcachefs_metadata_version_per_dev_fragmentation_lru;
		if (upgrading ||
		    fsck_err(trans, subvol_state_bad,
			     "subvolume state unset, recovering from legacy flags:\n%s",
			     (bch2_bkey_val_to_text(&buf, c, k), buf.buf))) {
			struct bkey_i_subvolume *n =
				errptr_try(bch2_bkey_make_mut_typed(trans, iter, &k, 0, subvolume));

			n->v.state = cpu_to_le32(bch2_subvolume_state_from_flags(&subvol));
			subvol = n->v;
		}
	}

	/*
	 * Pre-upgrade, the rewrite above always leaves a valid state, so this
	 * only fires post-upgrade - where any invalid value (including zero)
	 * is corruption. No repair yet, and state-keyed repairs must not run
	 * on a state we can't read:
	 */
	if (!bch2_subvolume_state_valid(bch2_subvolume_state(&subvol))) {
		unsigned dist;
		enum bch_subvolume_state nearest =
			bch2_subvolume_state_nearest(le32_to_cpu(subvol.state), &dist);

		/* bitflip correction, see check_snapshot */
		if (dist <= 2) {
			if (fsck_err(trans, subvol_state_bitflip,
				     "subvolume state 0x%x is a %u-bit flip of %s - correcting:\n%s",
				     le32_to_cpu(subvol.state), dist,
				     bch2_subvolume_state_str(nearest),
				     (bch2_bkey_val_to_text(&buf, c, k), buf.buf))) {
				struct bkey_i_subvolume *n =
					errptr_try(bch2_bkey_make_mut_typed(trans, iter, &k, 0, subvolume));
				bch2_subvolume_state_set(&n->v, nearest);
				subvol = n->v;
			}
		} else if (dist <= 6) {
			if (fsck_err(trans, subvol_state_bad,
				     "subvolume state 0x%x is %u bits from %s - correcting:\n%s",
				     le32_to_cpu(subvol.state), dist,
				     bch2_subvolume_state_str(nearest),
				     (bch2_bkey_val_to_text(&buf, c, k), buf.buf))) {
				struct bkey_i_subvolume *n =
					errptr_try(bch2_bkey_make_mut_typed(trans, iter, &k, 0, subvolume));
				bch2_subvolume_state_set(&n->v, nearest);
				subvol = n->v;
			}
		} else {
			CLASS(bch_log_msg, msg)(c);

			prt_printf(&msg.m, "subvolume has invalid state 0x%x (nearest codeword %s is %u bits away):\n",
				   le32_to_cpu(subvol.state), bch2_subvolume_state_str(nearest), dist);
			bch2_bkey_val_to_text(&msg.m, c, k);
			msg.m.suppress = !bch2_count_fsck_err(c, subvol_state_bad, &msg.m);

			return bch_err_throw(c, fsck_repair_unimplemented);
		}
	}

	/*
	 * A tombstone: it exists only as deletion's witness, reaped when its
	 * snapshot node is deleted - none of the live-subvolume invariants
	 * apply (the sweep may already have erased its root inode):
	 */
	if (bch2_subvolume_state_compat(&subvol) == SUBVOLUME_STATE_deleted)
		return 0;

	snapid = le32_to_cpu(subvol.snapshot);
	ret = bch2_snapshot_lookup(trans, snapid, &snapshot);

	if (bch2_err_matches(ret, ENOENT)) {
		bch2_log_msg_start(c, &buf);
		prt_printf(&buf, "subvolume points to missing snapshot\n");
		bch2_bkey_val_to_text(&buf, c, k);
		prt_newline(&buf);

		ret = bch2_run_explicit_recovery_pass(c, &buf,
					BCH_RECOVERY_PASS_reconstruct_snapshots, 0) ?: ret;
		bch2_print_str(c, KERN_NOTICE, buf.buf);
		return ret;
	}
	if (ret)
		return ret;

	/*
	 * Subvolumes only reference leaves; an interior target means a pointer
	 * was re-aimed by damage or a snapshot creation half-completed. No
	 * repair yet - the right re-aim (which descendant?) isn't decidable
	 * from this state alone. Checked before the unlinked branch:
	 * set_deleted here would mark an interior node will_delete:
	 */
	if (snapshot.children[0]) {
		CLASS(bch_log_msg, msg)(c);

		prt_printf(&msg.m, "subvolume points to interior snapshot node:\n");
		bch2_bkey_val_to_text(&msg.m, c, k);
		prt_newline(&msg.m);
		bch2_snapshot_to_text(&msg.m, &snapshot);
		msg.m.suppress = !bch2_count_fsck_err(c, subvol_snapshot_not_leaf, &msg.m);

		return bch_err_throw(c, fsck_repair_unimplemented);
	}

	if (bch2_subvolume_state_compat(&subvol) == SUBVOLUME_STATE_unlinked) {
		ret = bch2_subvolume_set_deleted(trans, iter->pos.offset);
		bch_err_msg(c, ret, "deleting subvolume %llu", iter->pos.offset);
		return ret ?: bch_err_throw(c, transaction_restart_nested);
	}

	/*
	 * A live subvolume's snapshot must be live, unconditionally - a
	 * fraudulent deletion state poisons every later pass that touches
	 * this snapshot's keys, whether or not the backref still agrees:
	 */
	if (bch2_snapshot_state_compat(&snapshot) != SNAPSHOT_STATE_live) {
		prt_printf(&buf, "subvolume points to a snapshot that isn't live:\n");
		bch2_bkey_val_to_text(&buf, c, k);
		prt_str(&buf, "\n");
		bch2_snapshot_to_text(&buf, &snapshot);
		bch_err(c, "%s", buf.buf);
		return bch_err_throw(c, EINVAL_snapshot_subvol_edge_bad);
	}

	if (le32_to_cpu(snapshot.subvol) != k.k->p.offset) {
		prt_printf(&buf, "subvolume points to a snapshot that doesn't point back:\n");
		bch2_bkey_val_to_text(&buf, c, k);
		prt_str(&buf, "\n");
		bch2_snapshot_to_text(&buf, &snapshot);
		bch_err(c, "%s", buf.buf);
		return bch_err_throw(c, EINVAL_snapshot_subvol_edge_bad);
	}

	if (fsck_err_on(k.k->p.offset == BCACHEFS_ROOT_SUBVOL &&
			subvol.fs_path_parent,
			trans, subvol_root_fs_path_parent_nonzero,
			"root subvolume has nonzero fs_path_parent\n%s",
			(bch2_bkey_val_to_text(&buf, c, k), buf.buf))) {
		struct bkey_i_subvolume *n =
			errptr_try(bch2_bkey_make_mut_typed(trans, iter, &k, 0, subvolume));

		n->v.fs_path_parent = 0;
		subvol = n->v;
	}

	if (subvol.fs_path_parent) {
		CLASS(btree_iter, subvol_children_iter)(trans,
					BTREE_ID_subvolume_children, subvolume_children_pos(k), 0);
		struct bkey_s_c subvol_children_k = bkey_try(bch2_btree_iter_peek_slot(&subvol_children_iter));

		if (fsck_err_on(subvol_children_k.k->type != KEY_TYPE_set,
				trans, subvol_children_not_set,
				"subvolume not set in subvolume_children btree at %llu:%llu\n%s",
				subvol_children_iter.pos.inode, subvol_children_iter.pos.offset,
				(printbuf_reset(&buf),
				 bch2_bkey_val_to_text(&buf, c, k), buf.buf))) {
			try(bch2_btree_bit_mod(trans, BTREE_ID_subvolume_children,
					       subvol_children_iter.pos, true));
		}
	}

	struct bch_inode_unpacked inode;
	ret = bch2_inode_find_by_inum_nowarn_trans(trans,
				    (subvol_inum) { k.k->p.offset, le64_to_cpu(subvol.inode) },
				    &inode);
	if (!ret) {
		if (fsck_err_on(inode.bi_subvol != k.k->p.offset,
				trans, subvol_root_wrong_bi_subvol,
				"subvol root %llu:%u has wrong bi_subvol field: got %u, should be %llu\n%s",
				inode.bi_inum, inode.bi_snapshot,
				inode.bi_subvol, k.k->p.offset,
				(printbuf_reset(&buf),
				 bch2_bkey_val_to_text(&buf, c, k),
				 prt_newline(&buf),
				 prt_printf(&buf, "snapshot %u: ", snapid),
				 bch2_snapshot_to_text(&buf, &snapshot),
				 prt_newline(&buf),
				 bch2_inode_unpacked_to_text(&buf, &inode),
				 buf.buf))) {
			inode.bi_subvol = k.k->p.offset;
			inode.bi_snapshot = le32_to_cpu(subvol.snapshot);
			try(__bch2_fsck_write_inode(trans, &inode));
		}
	} else if (bch2_err_matches(ret, ENOENT)) {
		if (fsck_err(trans, subvol_to_missing_root,
			     "subvolume %llu points to missing subvolume root %llu:%u",
			     k.k->p.offset, le64_to_cpu(subvol.inode),
			     le32_to_cpu(subvol.snapshot))) {
			/*
			 * Recreate - any contents that are still disconnected
			 * will then get reattached under lost+found
			 */
			bch2_inode_init_early(c, &inode);
			bch2_inode_init_late(c, &inode, bch2_current_time(c),
					     0, 0, S_IFDIR|0700, 0, NULL);
			inode.bi_inum			= le64_to_cpu(subvol.inode);
			inode.bi_snapshot		= le32_to_cpu(subvol.snapshot);
			inode.bi_subvol			= k.k->p.offset;
			inode.bi_parent_subvol		= le32_to_cpu(subvol.fs_path_parent);
			try(__bch2_fsck_write_inode(trans, &inode));
		}
	} else {
		return ret;
	}

	if (!BCH_SUBVOLUME_SNAP(&subvol)) {
		u32 snapshot_root = bch2_snapshot_root(c, le32_to_cpu(subvol.snapshot));
		u32 snapshot_tree = bch2_snapshot_tree(c, snapshot_root);

		struct bch_snapshot_tree st;
		ret = bch2_snapshot_tree_lookup(trans, snapshot_tree, &st);

		bch2_fs_inconsistent_on(bch2_err_matches(ret, ENOENT), c,
				"%s: snapshot tree %u not found", __func__, snapshot_tree);

		if (ret)
			return ret;

		if (fsck_err_on(le32_to_cpu(st.master_subvol) != k.k->p.offset,
				trans, subvol_not_master_and_not_snapshot,
				"subvolume %llu is not set as snapshot but is not master subvolume",
				k.k->p.offset)) {
			struct bkey_i_subvolume *s =
				errptr_try(bch2_bkey_make_mut_typed(trans, iter, &k, 0, subvolume));

			SET_BCH_SUBVOLUME_SNAP(&s->v, true);
		}
	}
fsck_err:
	return ret;
}

int bch2_check_subvols(struct bch_fs *c)
{
	CLASS(btree_trans, trans)(c);
	int ret = for_each_btree_key_commit(trans, iter,
				BTREE_ID_subvolumes, POS_MIN, BTREE_ITER_prefetch, k,
				NULL, NULL, BCH_TRANS_COMMIT_no_enospc,
			check_subvol(trans, &iter, k));

	/*
	 * If the pass completed cleanly the subvolumes btree is consistent;
	 * record it so check_key_has_snapshot can trust the in-memory table
	 * (see bch2_btree_is_clean). Same gate the pass runner uses to mark a
	 * pass complete.
	 */
	if (!ret && !test_bit(BCH_FS_error, &c->flags))
		bch2_set_btree_clean(c, BTREE_ID_subvolumes);
	return ret;
}

static int check_subvol_child(struct btree_trans *trans,
			      struct btree_iter *child_iter,
			      struct bkey_s_c child_k)
{
	struct bch_subvolume s;
	int ret = bch2_bkey_get_val_typed(trans, BTREE_ID_subvolumes, POS(0, child_k.k->p.offset),
					  0, subvolume, &s);
	if (ret && !bch2_err_matches(ret, ENOENT))
		return ret;

	if (fsck_err_on(ret ||
			bch2_subvolume_state_compat(&s) != SUBVOLUME_STATE_live ||
			le32_to_cpu(s.fs_path_parent) != child_k.k->p.inode,
			trans, subvol_children_bad,
			"incorrect entry in subvolume_children btree %llu:%llu",
			child_k.k->p.inode, child_k.k->p.offset))
		try(bch2_btree_delete_at(trans, child_iter, 0));

	/*
	 * A missing or deleted subvolume was the verdict (entry is stray,
	 * deleted above), not an error - don't fail the pass with it:
	 */
	ret = 0;
fsck_err:
	return ret;
}

int bch2_check_subvol_children(struct bch_fs *c)
{
	CLASS(btree_trans, trans)(c);
	return for_each_btree_key_commit(trans, iter,
				BTREE_ID_subvolume_children, POS_MIN, BTREE_ITER_prefetch, k,
				NULL, NULL, BCH_TRANS_COMMIT_no_enospc,
			check_subvol_child(trans, &iter, k));
}

/* Subvolumes: */

int bch2_subvolume_validate(struct bch_fs *c, struct bkey_s_c k,
			    const struct bkey_validate_context *from)
{
	struct bkey_s_c_subvolume subvol = bkey_s_c_to_subvolume(k);
	int ret = 0;

	bkey_fsck_err_on(bkey_lt(k.k->p, SUBVOL_POS_MIN) ||
			 bkey_gt(k.k->p, SUBVOL_POS_MAX),
			 c, subvol_pos_bad,
			 "invalid pos");

	bkey_fsck_err_on(!subvol.v->snapshot,
			 c, subvol_snapshot_bad,
			 "invalid snapshot");

	bkey_fsck_err_on(!subvol.v->inode,
			 c, subvol_inode_bad,
			 "invalid inode");

	if (bkey_val_bytes(k.k) > offsetof(struct bch_subvolume, pad))
		bkey_fsck_err_on(subvol.v->pad,
				 c, subvol_pad_nonzero,
				 "reserved pad field nonzero");

	/*
	 * Commit-only checks - defense in depth, never applied to existing
	 * keys (see bch2_snapshot_validate). The leaf check skips snapshot
	 * ids the table doesn't know: subvolume creation commits the
	 * subvolume in the same transaction as its new snapshot nodes, before
	 * the trigger has seen them:
	 */
	if (from->from == BKEY_VALIDATE_commit && !c->opts.no_commit_validate) {
		if (bkey_val_bytes(k.k) > offsetof(struct bch_subvolume, state))
			bkey_fsck_err_on(subvol.v->state &&
					 !bch2_subvolume_state_valid(bch2_subvolume_state(subvol.v)),
					 c, subvol_state_bad,
					 "invalid state 0x%x", le32_to_cpu(subvol.v->state));

		/* is_leaf < 0: id not in the table - skip, per above */
		bkey_fsck_err_on(bch2_snapshot_is_leaf(c, le32_to_cpu(subvol.v->snapshot)) == 0,
				 c, subvol_snapshot_not_leaf,
				 "snapshot %u is an interior node (subvolumes only reference leaves)",
				 le32_to_cpu(subvol.v->snapshot));
	}
fsck_err:
	return ret;
}

const char *bch2_subvolume_state_str(enum bch_subvolume_state s)
{
	switch (s) {
#define x(n, v) case SUBVOLUME_STATE_##n: return #n;
	BCH_SUBVOLUME_STATES()
#undef x
		default: return "(invalid state)";
	}
}

void bch2_subvolume_state_set(struct bch_subvolume *s, enum bch_subvolume_state n)
{
	/*
	 * There's only one legacy flag bit, but two non-live states (unlinked
	 * and deleted). Mirror *any* non-live state into it: recovering a wiped
	 * state field from the flags can't then tell unlinked from deleted, but
	 * both resolve to unlinked -> the deletion pipeline reruns and completes,
	 * rather than deriving a bare 'live' and reverting a pending deletion.
	 * (Also what an old kernel needs to see to keep deleting a tombstone.)
	 */
	SET_BCH_SUBVOLUME_UNLINKED_OBSOLETE(s, n != SUBVOLUME_STATE_live);
	s->state = cpu_to_le32(n);
}

__cold void bch2_subvolume_to_text(struct printbuf *out, struct bch_fs *c,
			    struct bkey_s_c k)
{
	struct bkey_s_c_subvolume s = bkey_s_c_to_subvolume(k);

	prt_printf(out, "root %llu snapshot id %u",
		   le64_to_cpu(s.v->inode),
		   le32_to_cpu(s.v->snapshot));

	if (bkey_val_bytes(s.k) > offsetof(struct bch_subvolume, creation_parent)) {
		prt_printf(out, " creation_parent %u", le32_to_cpu(s.v->creation_parent));
		prt_printf(out, " fs_parent %u", le32_to_cpu(s.v->fs_path_parent));
	}

	if (BCH_SUBVOLUME_RO(s.v))
		prt_printf(out, " ro");
	if (BCH_SUBVOLUME_SNAP(s.v))
		prt_printf(out, " snapshot");

	struct bch_subvolume v;
	bkey_val_copy_pad(&v, s);
	prt_printf(out, " %s", bch2_subvolume_state_str(bch2_subvolume_state_compat(&v)));
}

static int subvolume_children_mod(struct btree_trans *trans, struct bpos pos, bool set)
{
	return !bpos_eq(pos, POS_MIN)
		? bch2_btree_bit_mod(trans, BTREE_ID_subvolume_children, pos, set)
		: 0;
}

int bch2_subvolume_trigger(struct btree_trans *trans, struct btree_trigger_op op)
{
	if (op.flags & BTREE_TRIGGER_transactional) {
		/* The subvolumes btree is being mutated - it's no longer known clean: */
		bch2_clear_btree_clean(trans->c, BTREE_ID_subvolumes);

		struct bpos children_pos_old = subvolume_children_pos(op.old);
		struct bpos children_pos_new = subvolume_children_pos(op.new.s_c);

		if (!bpos_eq(children_pos_old, children_pos_new)) {
			try(subvolume_children_mod(trans, children_pos_old, false));
			try(subvolume_children_mod(trans, children_pos_new, true));
		}
	}

	return 0;
}

int bch2_subvol_has_children(struct btree_trans *trans, u32 subvol)
{
	CLASS(btree_iter, iter)(trans, BTREE_ID_subvolume_children, POS(subvol, 0), 0);
	struct bkey_s_c k = bch2_btree_iter_peek(&iter);

	return bkey_err(k) ?: k.k && k.k->p.inode == subvol
		? bch_err_throw(trans->c, ENOTEMPTY_subvol_not_empty)
		: 0;
}

static __always_inline int
bch2_subvolume_get_inlined(struct btree_trans *trans, unsigned subvol,
			   bool inconsistent_if_not_found,
			   struct bch_subvolume *s)
{
	int ret = bch2_bkey_get_val_typed(trans, BTREE_ID_subvolumes, POS(0, subvol),
					  BTREE_ITER_cached, subvolume, s);
	/*
	 * A deleted subvolume is a tombstone (deletion's witness, not yet
	 * reaped): to everyone but the deletion/reaping path it's gone, so
	 * report it as such rather than handing back a dead subvolume - and let
	 * it flow into the inconsistent_if_not_found handling below.
	 */
	if (!ret && bch2_subvolume_state_compat(s) == SUBVOLUME_STATE_deleted)
		ret = bch_err_throw(trans->c, ENOENT_subvolume_deleted);
	if (bch2_err_matches(ret, ENOENT) && inconsistent_if_not_found)
		ret = bch2_subvolume_missing(trans->c, subvol) ?: ret;
	return ret;
}

int bch2_subvolume_get(struct btree_trans *trans, unsigned subvol,
		       bool inconsistent_if_not_found,
		       struct bch_subvolume *s)
{
	return bch2_subvolume_get_inlined(trans, subvol, inconsistent_if_not_found, s);
}

/*
 * BCH_INODE_unlinked is allowed on a directory only if it's a subvolume
 * root and the subvolume is unlinked - this answers the second half.
 *
 * That's the only window fsck can see a legitimately flagged directory:
 * once the subvolume is tombstoned its snapshot is will_delete and
 * check_inode skips those keys - the sweep owns them. A tombstoned or
 * missing subvolume here is the caller's cue to repair, not exempt.
 *
 * Returns: < 0 error, 0 no, 1 yes
 */
int bch2_subvolume_is_unlinked(struct btree_trans *trans, u32 subvolid)
{
	struct bch_subvolume s;
	int ret = bch2_subvolume_get(trans, subvolid, false, &s);
	if (bch2_err_matches(ret, ENOENT))
		return 0;
	if (ret)
		return ret;

	return bch2_subvolume_state_compat(&s) == SUBVOLUME_STATE_unlinked;
}

int bch2_subvol_is_ro_trans(struct btree_trans *trans, u32 subvol, u32 *snapid)
{
	struct bch_subvolume s;
	try(bch2_subvolume_get_inlined(trans, subvol, true, &s));

	*snapid = le32_to_cpu(s.snapshot);

	if (BCH_SUBVOLUME_RO(&s) ||
	    bch2_subvolume_state_compat(&s) == SUBVOLUME_STATE_unlinked)
		return -EROFS;
	return 0;
}

int bch2_subvol_is_ro(struct bch_fs *c, u32 subvol)
{
	CLASS(btree_trans, trans)(c);
	u32 snapshot;
	return lockrestart_do(trans, bch2_subvol_is_ro_trans(trans, subvol, &snapshot));
}

int __bch2_subvolume_get_snapshot(struct btree_trans *trans, u32 subvolid,
				  u32 *snapid, bool warn)
{
	CLASS(btree_iter, iter)(trans, BTREE_ID_subvolumes, POS(0, subvolid), BTREE_ITER_cached);
	struct bkey_s_c_subvolume subvol = bch2_bkey_get_typed(&iter, subvolume);
	int ret = bkey_err(subvol);

	if (bch2_err_matches(ret, ENOENT))
		ret = bch2_subvolume_missing(trans->c, subvolid) ?: ret;

	if (likely(!ret))
		*snapid = le32_to_cpu(subvol.v->snapshot);
	return ret;
}

int bch2_subvolume_get_snapshot(struct btree_trans *trans, u32 subvolid,
				u32 *snapid)
{
	return __bch2_subvolume_get_snapshot(trans, subvolid, snapid, true);
}

static int bch2_subvolume_reparent(struct btree_trans *trans,
				   struct btree_iter *iter,
				   struct bkey_s_c k,
				   u32 old_parent, u32 new_parent)
{
	if (k.k->type != KEY_TYPE_subvolume)
		return 0;

	if (bkey_val_bytes(k.k) > offsetof(struct bch_subvolume, creation_parent) &&
	    le32_to_cpu(bkey_s_c_to_subvolume(k).v->creation_parent) != old_parent)
		return 0;

	struct bkey_i_subvolume *s =
		errptr_try(bch2_bkey_make_mut_typed(trans, iter, &k, 0, subvolume));

	s->v.creation_parent = cpu_to_le32(new_parent);
	return 0;
}

/*
 * Separate from the snapshot tree in the snapshots btree, we record the tree
 * structure of how snapshot subvolumes were created - the parent subvolume of
 * each snapshot subvolume.
 *
 * When a subvolume is deleted, we scan for child subvolumes and reparant them,
 * to avoid dangling references:
 */
static int bch2_subvolumes_reparent(struct btree_trans *trans, u32 subvolid_to_delete)
{
	struct bch_subvolume s;

	return lockrestart_do(trans,
			bch2_subvolume_get(trans, subvolid_to_delete, true, &s)) ?:
		for_each_btree_key_commit(trans, iter,
				BTREE_ID_subvolumes, POS_MIN, BTREE_ITER_prefetch, k,
				NULL, NULL, BCH_TRANS_COMMIT_no_enospc,
			bch2_subvolume_reparent(trans, &iter, k,
					subvolid_to_delete, le32_to_cpu(s.creation_parent)));
}

static int bch2_subvolume_set_state_trans(struct btree_trans *trans, u32 subvolid,
					  enum bch_subvolume_state state)
{
	struct bkey_i_subvolume *n =
		bch2_bkey_get_mut_typed(trans, BTREE_ID_subvolumes, POS(0, subvolid),
					BTREE_ITER_cached, subvolume);
	int ret = PTR_ERR_OR_ZERO(n);
	if (bch2_err_matches(ret, ENOENT))
		ret = bch2_subvolume_missing(trans->c, subvolid) ?: ret;
	if (unlikely(ret))
		return ret;

	bch2_subvolume_state_set(&n->v, state);
	n->v.fs_path_parent = 0;
	return ret;
}

/*
 * Delete subvolume, mark snapshot ID as deleted, queue up snapshot
 * deletion/cleanup:
 */
static int __bch2_subvolume_set_deleted(struct btree_trans *trans, u32 subvolid)
{
	CLASS(btree_iter, subvol_iter)(trans, BTREE_ID_subvolumes, POS(0, subvolid),
				       BTREE_ITER_cached|BTREE_ITER_intent);
	struct bkey_s_c_subvolume subvol = bch2_bkey_get_typed(&subvol_iter, subvolume);
	int ret = bkey_err(subvol);
	if (bch2_err_matches(ret, ENOENT))
		ret = bch2_subvolume_missing(trans->c, subvolid) ?: ret;
	if (ret)
		return ret;

	u32 snapid = le32_to_cpu(subvol.v->snapshot);

	CLASS(btree_iter, snapshot_iter)(trans, BTREE_ID_snapshots, POS(0, snapid), 0);
	struct bkey_s_c_snapshot snapshot = bch2_bkey_get_typed(&snapshot_iter, snapshot);
	ret = bkey_err(snapshot);
	bch2_fs_inconsistent_on(bch2_err_matches(ret, ENOENT), trans->c,
				"missing snapshot %u", snapid);
	if (ret)
		return ret;

	u32 treeid = le32_to_cpu(snapshot.v->tree);

	CLASS(btree_iter, snapshot_tree_iter)(trans, BTREE_ID_snapshot_trees, POS(0, treeid), 0);
	struct bkey_s_c_snapshot_tree snapshot_tree =
		bch2_bkey_get_typed(&snapshot_tree_iter, snapshot_tree);
	ret = bkey_err(snapshot_tree);
	bch2_fs_inconsistent_on(bch2_err_matches(ret, ENOENT), trans->c,
				"missing snapshot tree %u", treeid);
	if (ret)
		return ret;

	if (le32_to_cpu(snapshot_tree.v->master_subvol) == subvolid) {
		struct bkey_i_snapshot_tree *snapshot_tree_mut =
			errptr_try(bch2_bkey_make_mut_typed(trans, &snapshot_tree_iter,
						 &snapshot_tree.s_c,
						 0, snapshot_tree));

		snapshot_tree_mut->v.master_subvol = 0;
	}

	return  bch2_subvolume_set_state_trans(trans, subvolid, SUBVOLUME_STATE_deleted) ?:
		bch2_snapshot_node_set_deleted(trans, snapid);
}

static int bch2_subvolume_set_deleted(struct btree_trans *trans, u32 subvolid)
{
	/*
	 * The fsck caller (check_subvol's unlinked branch) can arrive with
	 * repairs - and their fsck_err journal log entries - still queued;
	 * reparent's lockrestart_do would drop them at trans_begin (the
	 * iter.c dropped-updates WARN). Flush first: commit_lazy is free
	 * when nothing is queued, and its restart-on-success re-drives
	 * check_subvol, whose committed fixes don't refire:
	 */
	int ret = bch2_trans_commit_lazy(trans, NULL, NULL, BCH_TRANS_COMMIT_no_enospc) ?:
		bch2_subvolumes_reparent(trans, subvolid) ?:
		commit_do(trans, NULL, NULL, BCH_TRANS_COMMIT_no_enospc,
			  __bch2_subvolume_set_deleted(trans, subvolid));
	return ret;
}

static void bch2_subvolume_wait_for_pagecache_and_delete(struct work_struct *work)
{
	struct bch_fs *c = container_of(work, struct bch_fs,
				snapshots.wait_for_pagecache_and_delete_work);
	int ret = 0;

	while (!ret) {
		snapshot_id_list s;

		scoped_guard(mutex, &c->snapshots.unlinked_lock) {
			s = c->snapshots.unlinked;
			darray_init(&c->snapshots.unlinked);
		}

		if (!s.nr)
			break;

		bch2_evict_subvolume_inodes(c, &s);

		CLASS(btree_trans, trans)(c);

		darray_for_each(s, id) {
			ret = bch2_subvolume_set_deleted(trans, *id);
			bch_err_msg(c, ret, "deleting subvolume %u", *id);
			if (ret)
				break;
		}

		darray_exit(&s);
	}

	enumerated_ref_put(&c->writes, BCH_WRITE_REF_snapshot_delete_pagecache);
}

struct subvolume_unlink_hook {
	struct btree_trans_commit_hook	h;
	u32				subvol;
};

static int bch2_subvolume_wait_for_pagecache_and_delete_hook(struct btree_trans *trans,
						      struct btree_trans_commit_hook *_h)
{
	struct subvolume_unlink_hook *h = container_of(_h, struct subvolume_unlink_hook, h);
	struct bch_fs *c = trans->c;

	scoped_guard(mutex, &c->snapshots.unlinked_lock)
		if (!snapshot_list_has_id(&c->snapshots.unlinked, h->subvol))
			try(snapshot_list_add(c, &c->snapshots.unlinked, h->subvol));

	if (!enumerated_ref_tryget(&c->writes, BCH_WRITE_REF_snapshot_delete_pagecache))
		return -EROFS;

	if (!queue_work(c->write_ref_wq, &c->snapshots.wait_for_pagecache_and_delete_work))
		enumerated_ref_put(&c->writes, BCH_WRITE_REF_snapshot_delete_pagecache);
	return 0;
}

int bch2_subvolume_unlink(struct btree_trans *trans, u32 subvolid)
{
	struct subvolume_unlink_hook *h = errptr_try(bch2_trans_kmalloc(trans, sizeof(*h)));

	h->h.fn		= bch2_subvolume_wait_for_pagecache_and_delete_hook;
	h->subvol	= subvolid;
	bch2_trans_commit_hook(trans, &h->h);

	try(bch2_subvolume_set_state_trans(trans, subvolid, SUBVOLUME_STATE_unlinked));

	/*
	 * We don't have an "unlinked subvolumes" btree" like we do for unlinked
	 * inodes, but subvolumes can also be held open by open file handles;
	 * schedule check_subvolumes to find any unlinked subvolumes and delete
	 * them if we crash after an unlink while they were still held open.
	 */
	bch2_recovery_pass_set_no_ratelimit(trans->c, BCH_RECOVERY_PASS_check_subvols);
	return 0;
}

int bch2_subvolume_create(struct btree_trans *trans, u64 inode,
			  u32 parent_subvolid,
			  u32 src_subvolid,
			  u32 *new_subvolid,
			  u32 *new_snapshotid,
			  struct bch_subvolume *new_subvol_out,
			  bool ro)
{
	struct bch_fs *c = trans->c;
	struct bkey_i_subvolume *new_subvol = NULL;
	struct bkey_i_subvolume *src_subvol = NULL;
	u32 parent = 0, new_nodes[2], snapshot_subvols[2];

	CLASS(btree_iter_uninit, dst_iter)(trans);
	int ret = bch2_bkey_get_empty_slot(trans, &dst_iter,
				BTREE_ID_subvolumes, POS_MIN, POS(0, U32_MAX));
	if (ret == -BCH_ERR_ENOSPC_btree_slot)
		ret = bch_err_throw(c, ENOSPC_subvolume_create);
	if (ret)
		return ret;

	snapshot_subvols[0] = dst_iter.pos.offset;
	snapshot_subvols[1] = src_subvolid;

	if (src_subvolid) {
		/* Creating a snapshot: */

		src_subvol = bch2_bkey_get_mut_typed(trans, BTREE_ID_subvolumes, POS(0, src_subvolid),
						     BTREE_ITER_cached, subvolume);
		ret = PTR_ERR_OR_ZERO(src_subvol);
		if (bch2_err_matches(ret, ENOENT))
			ret = bch2_subvolume_missing(trans->c, src_subvolid) ?: ret;
		if (unlikely(ret))
			return ret;

		parent = le32_to_cpu(src_subvol->v.snapshot);
	}

	try(bch2_snapshot_node_create(trans, parent, new_nodes,
				      snapshot_subvols,
				      src_subvolid ? 2 : 1));

	if (src_subvolid)
		src_subvol->v.snapshot = cpu_to_le32(new_nodes[1]);

	new_subvol = errptr_try(bch2_bkey_alloc(trans, &dst_iter, 0, subvolume));

	new_subvol->v.flags		= 0;
	new_subvol->v.snapshot		= cpu_to_le32(new_nodes[0]);
	new_subvol->v.inode		= cpu_to_le64(inode);
	new_subvol->v.creation_parent	= cpu_to_le32(src_subvolid);
	new_subvol->v.fs_path_parent	= cpu_to_le32(parent_subvolid);
	new_subvol->v.otime.lo		= cpu_to_le64(bch2_current_time(c));
	new_subvol->v.otime.hi		= 0;

	SET_BCH_SUBVOLUME_RO(&new_subvol->v, ro);
	SET_BCH_SUBVOLUME_SNAP(&new_subvol->v, src_subvolid != 0);
	bch2_subvolume_state_set(&new_subvol->v, SUBVOLUME_STATE_live);

	*new_subvolid	= new_subvol->k.p.offset;
	*new_snapshotid	= new_nodes[0];
	*new_subvol_out	= new_subvol->v;
	return 0;
}

int bch2_initialize_subvolumes(struct bch_fs *c)
{
	struct bkey_i_snapshot_tree	root_tree;
	struct bkey_i_snapshot		root_snapshot;
	struct bkey_i_subvolume		root_volume;

	bkey_snapshot_tree_init(&root_tree.k_i);
	root_tree.k.p.offset		= 1;
	root_tree.v.master_subvol	= cpu_to_le32(1);
	root_tree.v.root_snapshot	= cpu_to_le32(U32_MAX);

	bkey_snapshot_init(&root_snapshot.k_i);
	root_snapshot.k.p.offset = U32_MAX;
	root_snapshot.v.flags	= 0;
	root_snapshot.v.parent	= 0;
	root_snapshot.v.subvol	= cpu_to_le32(BCACHEFS_ROOT_SUBVOL);
	root_snapshot.v.tree	= cpu_to_le32(1);
	bch2_snapshot_state_set(&root_snapshot.v, SNAPSHOT_STATE_live);

	bkey_subvolume_init(&root_volume.k_i);
	root_volume.k.p.offset = BCACHEFS_ROOT_SUBVOL;
	root_volume.v.flags	= 0;
	root_volume.v.snapshot	= cpu_to_le32(U32_MAX);
	root_volume.v.inode	= cpu_to_le64(BCACHEFS_ROOT_INO);
	bch2_subvolume_state_set(&root_volume.v, SUBVOLUME_STATE_live);

	return  bch2_btree_insert(c, BTREE_ID_snapshot_trees,	&root_tree.k_i, NULL, 0, 0) ?:
		bch2_btree_insert(c, BTREE_ID_snapshots,	&root_snapshot.k_i, NULL, 0, 0) ?:
		bch2_btree_insert(c, BTREE_ID_subvolumes,	&root_volume.k_i, NULL, 0, 0);
}

static int __bch2_fs_upgrade_for_subvolumes(struct btree_trans *trans)
{
	CLASS(btree_iter, iter)(trans, BTREE_ID_inodes, SPOS(0, BCACHEFS_ROOT_INO, U32_MAX), 0);
	struct bkey_s_c k = bch2_btree_iter_peek_slot(&iter);
	int ret = bkey_err(k);
	if (ret)
		return ret;

	if (!bkey_is_inode(k.k)) {
		struct bch_fs *c = trans->c;
		bch_err(c, "root inode not found");
		return bch_err_throw(c, ENOENT_inode);
	}

	struct bch_inode_unpacked inode;
	bch2_inode_unpack(trans->c, k, &inode);

	inode.bi_subvol = BCACHEFS_ROOT_SUBVOL;

	return bch2_inode_write(trans, &iter, &inode);
}

/* set bi_subvol on root inode */
int bch2_fs_upgrade_for_subvolumes(struct bch_fs *c)
{
	CLASS(btree_trans, trans)(c);
	return commit_do(trans, NULL, NULL, BCH_TRANS_COMMIT_no_enospc,
			    __bch2_fs_upgrade_for_subvolumes(trans));
}

void bch2_fs_subvolumes_init_early(struct bch_fs *c)
{
	INIT_WORK(&c->snapshots.wait_for_pagecache_and_delete_work,
		  bch2_subvolume_wait_for_pagecache_and_delete);
}

