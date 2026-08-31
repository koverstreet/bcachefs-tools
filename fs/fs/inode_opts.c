// SPDX-License-Identifier: GPL-2.0
/*
 * Per-inode io path options: storage, resolution against filesystem defaults,
 * inheritance, propagation to ancestor snapshots.
 *
 * Stored with a +1 bias: 0 is unset (inherit), 1 is an explicit "none".
 * Nothing outside this file should touch bi_<option> directly.
 */

#include "bcachefs.h"

#include "btree/update.h"

#include "data/compress.h"
#include "data/reconcile/work.h"

#include "fs/inode.h"
#include "fs/inode_opts.h"
#include "fs/logged_ops.h"

#include "init/error.h"

#include "snapshots/snapshot.h"
#include "snapshots/subvolume.h"

#define x(name, ...)	#name,
const char * const bch2_inode_opts[] = {
	BCH_INODE_OPTS()
	NULL,
};
#undef x

int bch2_opt_to_inode_opt(int id)
{
	switch (id) {
#define x(name, ...)				\
	case Opt_##name: return Inode_opt_##name;
	BCH_INODE_OPTS()
#undef  x
	default:
		return -1;
	}
}

/*
 * Copy the options @dst inherits from @src - everything it hasn't set for
 * itself. Called when a file moves between directories.
 */
bool bch2_reinherit_attrs(struct bch_inode_unpacked *dst_u,
			  struct bch_inode_unpacked *src_u)
{
	bool ret = false;

	for (unsigned id = 0; id < Inode_opt_nr; id++) {
		if (!S_ISDIR(dst_u->bi_mode) && id == Inode_opt_casefold)
			continue;

		if (dst_u->bi_fields_set & (1 << id))
			continue;

		u64 src = bch2_inode_opt_get(src_u, id);
		u64 dst = bch2_inode_opt_get(dst_u, id);

		if (src == dst)
			continue;

		bch2_inode_opt_set(dst_u, id, src);
		ret = true;
	}

	return ret;
}

struct bch_opts bch2_inode_opts_to_opts(struct bch_inode_unpacked *inode)
{
	struct bch_opts ret = { 0 };
#define x(_name, _bits)							\
	if (inode->bi_##_name)						\
		opt_set(ret, _name, inode->bi_##_name - 1);
	BCH_INODE_OPTS()
#undef x
	return ret;
}

void bch2_inode_opts_get_inode(struct bch_fs *c,
			       struct bch_inode_unpacked *inode,
			       struct bch_inode_opts *ret)
{
#define x(_name, _bits)							\
	if ((inode)->bi_##_name) {					\
		ret->_name = inode->bi_##_name - 1;			\
		ret->_name##_from_inode = true;				\
	} else {							\
		ret->_name = c->opts._name;				\
		ret->_name##_from_inode = false;			\
	}
	BCH_INODE_OPTS()
#undef x

	/*
	 * Forward compatibility: inodes written by newer versions may carry
	 * checksum/compression types we don't know about — fall back to the
	 * filesystem option for new writes. Reads are unaffected, extents
	 * carry their own types. (This is why these aren't validated at
	 * btree read time: that would reject valid inodes from newer
	 * versions.)
	 */
	if (unlikely(ret->data_checksum >= BCH_CSUM_OPT_NR)) {
		ret->data_checksum = c->opts.data_checksum;
		ret->data_checksum_from_inode = false;
	}
	if (unlikely(!bch2_compression_opt_valid(ret->compression))) {
		ret->compression = c->opts.compression;
		ret->compression_from_inode = false;
	}
	if (unlikely(!bch2_compression_opt_valid(ret->background_compression))) {
		ret->background_compression = c->opts.background_compression;
		ret->background_compression_from_inode = false;
	}

	ret->change_cookie = c->opt_change_cookie;

	bch2_io_opts_fixups(ret);
}

/*
 * Per option, the strongest value held by an inode version below @ancestor and
 * off the path from @origin_snapshot; 0 if none.
 *
 * A snapshot with no inode key isn't seen, deliberately: it reads @ancestor,
 * so it should follow the change.
 */
static int inode_opts_claimed_off_path(struct btree_trans *trans, u64 inum,
				       u32 ancestor, u32 origin_snapshot, u64 *claimed)
{
	struct bkey_s_c k;
	int ret = 0;

	memset(claimed, 0, sizeof(claimed[0]) * Inode_opt_nr);

	for_each_btree_key_max_norestart(trans, iter, BTREE_ID_inodes,
					 SPOS(0, inum, 0), SPOS(0, inum, U32_MAX),
					 BTREE_ITER_all_snapshots, k, ret) {
		u32 snapshot = k.k->p.snapshot;

		if (!bkey_is_inode(k.k) ||
		    snapshot == ancestor ||
		    !bch2_snapshot_is_ancestor(trans, snapshot, ancestor) ||
		    bch2_snapshot_is_ancestor(trans, origin_snapshot, snapshot))
			continue;

		struct bch_inode_unpacked inode;
		bch2_inode_unpack(trans->c, k, &inode);

		for (enum inode_opt_id id = 0; id < Inode_opt_nr; id++)
			claimed[id] = max(claimed[id], bch2_inode_opt_get(&inode, id));
	}

	return ret;
}

/*
 * Is @snapshot the master subvolume - the one in a tree that isn't itself a
 * snapshot? check_snapshot_tree() maintains that.
 *
 * A tree need not have one: set_deleted() clears master_subvol and only fsck
 * elects a replacement. Not damage, just no tiebreak - hence ENOENT is false,
 * not an error.
 */
static int snapshot_is_master_subvol(struct btree_trans *trans, u32 snapshot, bool *ret)
{
	*ret = false;

	struct bch_snapshot_tree st;
	int r = bch2_snapshot_tree_lookup(trans, bch2_snapshot_tree(trans->c, snapshot), &st);
	if (bch2_err_matches(r, ENOENT))
		return 0;
	try(r);

	if (!st.master_subvol)
		return 0;

	struct bch_subvolume subvol;
	r = bch2_subvolume_get(trans, le32_to_cpu(st.master_subvol), false, &subvol);
	if (bch2_err_matches(r, ENOENT))
		return 0;
	try(r);

	*ret = le32_to_cpu(subvol.snapshot) == snapshot;
	return 0;
}

/*
 * Which branch wins: see the Principles of Operation.
 *
 * data_replicas maxes over @src and claims, not @dst - that would ratchet.
 * @dst agreeing skips the write, not the climb.
 */
static bool inode_opts_merge_into_ancestor(struct bch_inode_unpacked *src,
					   struct bch_inode_unpacked *dst,
					   u64 *claimed, bool src_is_master)
{
	bool changed = false;

	for (enum inode_opt_id id = 0; id < Inode_opt_nr; id++) {
		u64 v = bch2_inode_opt_get(src, id);
		if (!v)
			continue;

		if (id == Inode_opt_data_replicas)
			v = max(v, claimed[id]);
		else if (claimed[id] && !src_is_master)
			continue;

		if (bch2_inode_opt_get(dst, id) == v)
			continue;

		bch2_inode_opt_set(dst, id, v);
		changed = true;
	}

	return changed;
}

/*
 * Queue what @ancestor should take from the version at @origin_snapshot.
 *
 * @done means the whole op is moot - the source inode was unlinked - not that
 * this level had nothing to do.
 *
 * @fsck_repaired non-NULL reports before writing, and says whether we did; the
 * check is then the same code as the rule.
 */
static int inode_opt_propagate_one(struct btree_trans *trans, u64 inum,
				   u32 origin_snapshot, u32 ancestor, bool *done,
				   bool *fsck_repaired)
{
	struct bch_fs *c = trans->c;
	struct bch_inode_unpacked src;
	CLASS(printbuf, buf)();
	int ret = 0;

	CLASS(btree_iter, src_iter)(trans, BTREE_ID_inodes, SPOS(0, inum, origin_snapshot),
				    BTREE_ITER_all_snapshots);
	struct bkey_s_c src_k = bch2_btree_iter_peek_slot(&src_iter);
	try(bkey_err(src_k));
	if (!bkey_is_inode(src_k.k)) {
		*done = true;
		return 0;
	}

	bch2_inode_unpack(c, src_k, &src);

	CLASS(btree_iter, iter)(trans, BTREE_ID_inodes, SPOS(0, inum, ancestor),
				BTREE_ITER_intent|BTREE_ITER_all_snapshots);
	struct bkey_s_c k = bch2_btree_iter_peek_slot(&iter);
	try(bkey_err(k));
	if (!bkey_is_inode(k.k))
		return 0;

	struct bch_inode_unpacked ancestor_inode;
	bch2_inode_unpack(c, k, &ancestor_inode);

	u64 claimed[Inode_opt_nr];
	try(inode_opts_claimed_off_path(trans, inum, ancestor, origin_snapshot, claimed));

	bool src_is_master;
	try(snapshot_is_master_subvol(trans, origin_snapshot, &src_is_master));

	if (!inode_opts_merge_into_ancestor(&src, &ancestor_inode, claimed, src_is_master))
		return 0;

	if (fsck_repaired) {
		/*
		 * Repair quietly until the compat bit says a full check has
		 * run: before that the filesystem predates propagation, so
		 * finding work is the upgrade, not a bug.
		 */
		unsigned flags = FSCK_CAN_FIX|FSCK_CAN_IGNORE;
		if (!(c->sb.compat & BIT_ULL(BCH_COMPAT_inode_opts_propagated)))
			flags |= FSCK_ERR_SILENT;

		prt_printf(&buf, "inode options not propagated to ancestor snapshot\n");
		prt_printf(&buf, "inum %llu set at snapshot %u, ancestor %u should be:\n",
			   inum, origin_snapshot, ancestor);
		bch2_inode_unpacked_to_text(&buf, &ancestor_inode);

		if (!__fsck_err(trans, flags, inode_opts_not_propagated, "%s", buf.buf))
			return 0;

		*fsck_repaired = true;
	}

	/* in place: the normal update path would COW it down into a leaf */
	ret = bch2_inode_write_flags(trans, &iter, &ancestor_inode,
				     BTREE_UPDATE_internal_snapshot_node);
fsck_err:
	return ret;
}

void bch2_logged_op_inode_opt_propagate_to_text(struct printbuf *out, struct bch_fs *c,
						struct bkey_s_c k)
{
	const struct bch_logged_op_inode_opt_propagate *op =
		bkey_s_c_to_logged_op_inode_opt_propagate(k).v;

	prt_printf(out, "inum=%llu origin=%u cursor=%u",
		   le64_to_cpu(op->inum),
		   le32_to_cpu(op->origin_snapshot),
		   le32_to_cpu(op->cursor_snapshot));
}

int bch2_resume_logged_op_inode_opt_propagate(struct btree_trans *trans,
					      struct bkey_i *op_k)
{
	struct bch_fs *c = trans->c;
	struct bkey_i_logged_op_inode_opt_propagate *op =
		bkey_i_to_logged_op_inode_opt_propagate(op_k);
	u64 inum   = le64_to_cpu(op->v.inum);
	u32 origin_snapshot = le32_to_cpu(op->v.origin_snapshot);

	while (1) {
		bch2_trans_begin(trans);

		u32 ancestor = bch2_snapshot_parent(c, le32_to_cpu(op->v.cursor_snapshot));
		if (!ancestor)
			break;

		/* advanced in the same commit as the work it describes */
		op->v.cursor_snapshot = cpu_to_le32(ancestor);

		bool done = false;
		try(commit_do(trans, NULL, NULL, BCH_TRANS_COMMIT_no_enospc,
			      inode_opt_propagate_one(trans, inum, origin_snapshot,
						      ancestor, &done, NULL) ?:
			      bch2_logged_op_update(trans, &op->k_i)));
		if (done)
			break;
	}

	/*
	 * The "after" half of the opt change bracket, which for an inode option
	 * lands here, not when setxattr returns: a scan before the ancestors
	 * carry the new options would find nothing to do.
	 */
	bch2_trans_begin(trans);
	try(commit_do(trans, NULL, NULL, BCH_TRANS_COMMIT_no_enospc,
		      bch2_set_reconcile_needs_scan_trans(trans,
				(struct reconcile_scan) {
					.type	= RECONCILE_SCAN_inum,
					.inum	= inum,
				})));
	bch2_reconcile_wakeup(c);
	return 0;
}

/*
 * Goes in the caller's transaction, so the op commits with the change that
 * needs it: a crash can't leave the option set with no climb recorded.
 *
 * @op is left deleted if there's nothing above @snapshot; the caller runs
 * _finish() only if it was armed, and must keep it alive until the commit.
 */
int bch2_inode_opt_propagate_start(struct btree_trans *trans, u64 inum, u32 snapshot,
				   struct bkey_i_logged_op_inode_opt_propagate *op)
{
	bkey_init(&op->k);

	if (!bch2_snapshot_parent(trans->c, snapshot))
		return 0;

	bkey_logged_op_inode_opt_propagate_init(&op->k_i);
	op->v.inum		= cpu_to_le64(inum);
	op->v.origin_snapshot	= cpu_to_le32(snapshot);
	op->v.cursor_snapshot	= cpu_to_le32(snapshot);

	return __bch2_logged_op_start(trans, &op->k_i);
}

/* After that transaction has committed: climb, then drop the op */
int bch2_inode_opt_propagate_finish(struct btree_trans *trans,
				    struct bkey_i_logged_op_inode_opt_propagate *op)
{
	int ret = bch2_resume_logged_op_inode_opt_propagate(trans, &op->k_i);
	return bch2_logged_op_finish(trans, &op->k_i, ret) ?: ret;
}

/*
 * An invariant, not a one-off migration: deleting one of two disagreeing
 * branches leaves the survivor's value uncontested but never propagated, and
 * nothing re-runs the climb on snapshot deletion.
 *
 * No logged op - fsck reruns from the start anyway, and bch2_trans_begin() in
 * its climb would invalidate check_inodes()'s iterator.
 */
int bch2_check_inode_opts_propagated(struct btree_trans *trans,
				     struct bch_inode_unpacked *inode)
{
	struct bch_fs *c = trans->c;
	bool repaired = false;

	if (!(inode->bi_flags & BCH_INODE_has_inode_opts))
		return 0;

	u32 ancestor = inode->bi_snapshot;
	while ((ancestor = bch2_snapshot_parent(c, ancestor))) {
		bool done = false;

		try(inode_opt_propagate_one(trans, inode->bi_inum, inode->bi_snapshot,
					    ancestor, &done, &repaired));
		if (done)
			break;
	}

	/*
	 * We've written keys outside the one check_inodes() handed us, at
	 * snapshots it isn't iterating; commit them and restart so it resumes
	 * from a consistent position.
	 */
	if (repaired)
		return bch2_trans_commit(trans, NULL, NULL, BCH_TRANS_COMMIT_no_enospc) ?:
			btree_trans_restart(trans, BCH_ERR_transaction_restart_nested);

	return 0;
}
