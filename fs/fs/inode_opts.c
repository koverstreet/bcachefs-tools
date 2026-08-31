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
 * Is there an inode version below @ancestor, off the path from @ours to
 * @ancestor, that has its own value for @id?
 *
 * Such a branch doesn't read @ancestor for this option, so propagating into
 * @ancestor wouldn't change what it sees - but it does mean two branches have
 * expressed different intentions for the same shared data, and picking one
 * here would be deciding that silently.
 *
 * A snapshot that has never had this inode modified has no inode key at all,
 * so it isn't seen here. That is deliberate: it reads @ancestor, and having it
 * follow the change is the entire point of propagating.
 */
static int inode_opts_claimed_off_path(struct btree_trans *trans, u64 inum,
				       u32 ancestor, u32 origin_snapshot, unsigned *claimed)
{
	struct bkey_s_c k;
	int ret = 0;

	*claimed = 0;

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
			if (bch2_inode_opt_get(&inode, id))
				*claimed |= BIT(id);
	}

	return ret;
}

/*
 * Extents are keyed at the snapshot they were written at, and their io options
 * come from the inode version at the nearest ancestor of that snapshot
 * (bch2_bkey_get_io_opts()) - so options set in a branch don't reach data
 * written before the branch existed, which is why background_compression looks
 * like it does nothing on a snapshotted file until the snapshot is deleted.
 *
 * Queue whatever @ancestor should take from the version at @origin_snapshot. @done
 * means the whole op is moot, not that this level had nothing to do: the
 * source inode can be unlinked while the op is outstanding. A deleted snapshot
 * needs no check - bch2_snapshot_parent() returns 0 for an id with no table
 * entry.
 */
static int inode_opt_propagate_one(struct btree_trans *trans, u64 inum,
				   u32 origin_snapshot, u32 ancestor, bool *done)
{
	struct bch_fs *c = trans->c;
	struct bch_inode_unpacked src;

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

	unsigned claimed;
	try(inode_opts_claimed_off_path(trans, inum, ancestor, origin_snapshot, &claimed));

	bool changed = false;
	for (enum inode_opt_id id = 0; id < Inode_opt_nr; id++) {
		u64 v = bch2_inode_opt_get(&src, id);

		/*
		 * An ancestor that already agrees is a reason to skip the
		 * write, not to stop climbing: the level above may still need
		 * this, and the value here may have been set here rather than
		 * propagated from below.
		 */
		if (!v ||
		    (claimed & BIT(id)) ||
		    bch2_inode_opt_get(&ancestor_inode, id) == v)
			continue;

		bch2_inode_opt_set(&ancestor_inode, id, v);
		changed = true;
	}

	/*
	 * internal_snapshot_node: modifying the ancestor in place is the whole
	 * point - the normal update path would copy it down into a leaf, which
	 * is what we're trying to avoid.
	 */
	return changed
		? bch2_inode_write_flags(trans, &iter, &ancestor_inode,
					 BTREE_UPDATE_internal_snapshot_node)
		: 0;
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
						      ancestor, &done) ?:
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
 * Not atomic with the option change: a crash in between leaves the option set
 * and the climb never started. Accepted - the op only guarantees that a climb
 * which has begun finishes.
 */
int bch2_inode_opt_propagate(struct btree_trans *trans, subvol_inum inum)
{
	u32 snapshot;
	try(lockrestart_do(trans,
		bch2_subvolume_get_snapshot(trans, inum.subvol, &snapshot)));

	/* No ancestors: nothing to climb, and don't pay for a logged op */
	if (!bch2_snapshot_parent(trans->c, snapshot))
		return 0;

	/* Stack, not trans_kmalloc: start() commits, and a restart frees it */
	struct bkey_i_logged_op_inode_opt_propagate op;

	bkey_logged_op_inode_opt_propagate_init(&op.k_i);
	op.v.inum		= cpu_to_le64(inum.inum);
	op.v.origin_snapshot	= cpu_to_le32(snapshot);
	op.v.cursor_snapshot	= cpu_to_le32(snapshot);

	try(bch2_logged_op_start(trans, &op.k_i));
	int ret = bch2_resume_logged_op_inode_opt_propagate(trans, &op.k_i);
	return bch2_logged_op_finish(trans, &op.k_i) ?: ret;
}
