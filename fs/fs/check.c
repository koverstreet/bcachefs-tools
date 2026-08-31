// SPDX-License-Identifier: GPL-2.0
#include "bcachefs.h"
#include "bcachefs_ioctl.h"

#include "alloc/buckets.h"

#include "btree/bkey_buf.h"
#include "btree/cache.h"
#include "btree/update.h"

#include "fs/dirent.h"
#include "fs/check.h"
#include "fs/inode.h"
#include "fs/namei.h"
#include "fs/xattr.h"

#include "init/error.h"
#include "init/progress.h"
#include "init/passes.h"
#include "init/fs.h"

#include "snapshots/snapshot.h"

#include "vfs/fs.h"

#include "util/darray.h"
#include "util/thread_with_file.h"

#include <linux/dcache.h> /* struct qstr */

void bch2_dirent_inode_mismatch_msg(struct printbuf *out, struct bch_fs *c,
				    struct bkey_s_c_dirent dirent,
				    struct bch_inode_unpacked *inode)
{
	prt_str(out, "inode points to dirent that does not point back:");
	prt_newline(out);
	bch2_bkey_val_to_text(out, c, dirent.s_c);
	prt_newline(out);
	bch2_inode_unpacked_to_text(out, inode);
}

static s64 bch2_count_subdirs(struct btree_trans *trans, u64 inum,
				    u32 snapshot)
{
	u64 subdirs = 0;

	int ret = for_each_btree_key_max(trans, iter, BTREE_ID_dirents,
				    SPOS(inum, 0, snapshot),
				    POS(inum, U64_MAX),
				    0, k, ({
		if (k.k->type == KEY_TYPE_dirent &&
		    bkey_s_c_to_dirent(k).v->d_type == DT_DIR)
			subdirs++;
		0;
	}));

	return ret ?: subdirs;
}

static int subvol_lookup(struct btree_trans *trans, u32 subvol,
			 u32 *snapshot, u64 *inum)
{
	struct bch_subvolume s;
	int ret = bch2_subvolume_get(trans, subvol, false, &s);

	*snapshot = le32_to_cpu(s.snapshot);
	*inum = le64_to_cpu(s.inode);
	return ret;
}

static int lookup_dirent_in_snapshot(struct btree_trans *trans,
			   struct bch_hash_info hash_info,
			   subvol_inum dir, struct qstr *name,
			   u64 *target, unsigned *type, u32 snapshot)
{
	CLASS(btree_iter_uninit, iter)(trans);
	struct bkey_s_c k = bkey_try(bch2_hash_lookup_in_snapshot(trans, &iter, bch2_dirent_hash_desc,
							 &hash_info, dir, name, 0, snapshot));

	struct bkey_s_c_dirent d = bkey_s_c_to_dirent(k);
	*target = le64_to_cpu(d.v->d_inum);
	*type = d.v->d_type;
	return 0;
}

/*
 * Find any subvolume associated with a tree of snapshots
 * We can't rely on master_subvol - it might have been deleted.
 */
static int find_snapshot_tree_subvol(struct btree_trans *trans,
				     u32 tree_id, u32 *subvol)
{
	struct bkey_s_c k;
	int ret;

	for_each_btree_key_norestart(trans, iter, BTREE_ID_snapshots, POS_MIN, 0, k, ret) {
		if (k.k->type != KEY_TYPE_snapshot)
			continue;

		struct bkey_s_c_snapshot s = bkey_s_c_to_snapshot(k);
		if (le32_to_cpu(s.v->tree) != tree_id)
			continue;

		if (s.v->subvol) {
			*subvol = le32_to_cpu(s.v->subvol);
			return 0;
		}
	}

	return ret ?: bch_err_throw(trans->c, ENOENT_no_snapshot_tree_subvol);
}

static struct qstr lostfound_str = QSTR("lost+found");

static int create_lostfound(struct btree_trans *trans, u32 snapshot,
			    subvol_inum root_inum,
			    struct bch_inode_unpacked *root_inode,
			    struct bch_inode_unpacked *lostfound)
{
	struct bch_fs *c = trans->c;

	CLASS(bch_log_msg_level, msg)(c, LOGLEVEL_notice);
	prt_printf(&msg.m, "creating ");
	try(bch2_inum_to_path(trans, root_inum, &msg.m));
	prt_printf(&msg.m, "/lost+found in subvol %llu snapshot %u", root_inum.subvol, snapshot);

	u64 now = bch2_current_time(c);

	bch2_inode_init_early(c, lostfound);
	bch2_inode_init_late(c, lostfound, now, 0, 0, S_IFDIR|0700, 0, root_inode);
	lostfound->bi_dir = root_inode->bi_inum;
	lostfound->bi_snapshot = snapshot;

	CLASS(btree_iter_uninit, lostfound_iter)(trans);
	try(bch2_inode_create(trans, &lostfound_iter, lostfound, snapshot,
			      inode_opt_get(c, root_inode, inodes_32bit)));

	bch2_btree_iter_set_snapshot(&lostfound_iter, snapshot);
	try(bch2_btree_iter_traverse(&lostfound_iter));

	int ret = bch2_dirent_create_snapshot(trans,
				root_inum.subvol, snapshot, root_inode,
				mode_to_type(lostfound->bi_mode),
				&lostfound_str,
				lostfound->bi_inum,
				&lostfound->bi_dir_offset,
				BTREE_UPDATE_internal_snapshot_node|
				STR_HASH_must_create);
	if (ret) {
		if (!bch2_err_matches(ret, BCH_ERR_transaction_restart)) {
			msg.loglevel = LOGLEVEL_err;
			prt_printf(&msg.m, "\nerror creating dirent: %s", bch2_err_str(ret));
		}
		return ret;
	}

	return bch2_inode_write_flags(trans, &lostfound_iter, lostfound,
				      BTREE_UPDATE_internal_snapshot_node);
}

/*
 * The snapshot tree has a lost+found, but @snapshot hasn't: it was deleted
 * there. Give @snapshot a dirent for the same inode - one lost+found per tree
 * is the invariant, not one dirent.
 *
 * A fresh dirent, deliberately, rather than removing the whiteout in place:
 * dirents are a hash table, so let the table pick the slot. It goes back into
 * the slot that was freed in the normal case, and probes past whatever took it
 * otherwise. One inode with different dirent positions in different snapshots
 * is what a directory renamed after a snapshot already looks like.
 */
static int restore_lostfound(struct btree_trans *trans, u32 snapshot,
			     u32 root_snapshot,
			     subvol_inum root_inum,
			     struct bch_inode_unpacked *root_inode,
			     u64 inum, unsigned d_type,
			     struct bch_inode_unpacked *lostfound)
{
	struct bch_fs *c = trans->c;

	if (d_type != DT_DIR) {
		bch_err(c, "lost+found in snapshot %u is not a directory (type %u), cannot restore it in snapshot %u",
			root_snapshot, d_type, snapshot);
		return bch_err_throw(c, ENOENT_not_directory);
	}

	/*
	 * The inode is normally still visible here - only the dirent was
	 * shadowed - but if the directory was deleted rather than unlinked it's
	 * shadowed too, and we have to go get it from the root snapshot.
	 */
	int ret = bch2_inode_find_by_inum_snapshot(trans, inum, snapshot, lostfound, 0);
	if (bch2_err_matches(ret, ENOENT))
		ret = bch2_inode_find_by_inum_snapshot(trans, inum, root_snapshot, lostfound, 0);
	if (ret) {
		bch_err_msg(c, ret, "looking up lost+found inode %llu in snapshot %u or %u",
			    inum, snapshot, root_snapshot);
		return ret;
	}

	CLASS(bch_log_msg_level, msg)(c, LOGLEVEL_notice);
	prt_printf(&msg.m, "restoring ");
	try(bch2_inum_to_path(trans, root_inum, &msg.m));
	prt_printf(&msg.m, "/lost+found in subvol %llu snapshot %u: inode %llu, deleted here, still in snapshot %u",
		   root_inum.subvol, snapshot, inum, root_snapshot);

	lostfound->bi_dir = root_inode->bi_inum;
	lostfound->bi_snapshot = snapshot;

	ret = bch2_dirent_create_snapshot(trans,
				root_inum.subvol, snapshot, root_inode,
				d_type,
				&lostfound_str,
				inum,
				&lostfound->bi_dir_offset,
				BTREE_UPDATE_internal_snapshot_node|
				STR_HASH_must_create);
	if (ret) {
		if (!bch2_err_matches(ret, BCH_ERR_transaction_restart)) {
			msg.loglevel = LOGLEVEL_err;
			prt_printf(&msg.m, "\nerror creating dirent: %s", bch2_err_str(ret));
		}
		return ret;
	}

	return __bch2_fsck_write_inode(trans, lostfound);
}

/*
 * lost+found is a subdirectory of the root inode in @snapshot, so the root
 * inode gains a link there. Take it on the version @snapshot sees and write it
 * back at @snapshot: writing it where that version lives would hand the link to
 * sibling branches that haven't got a lost+found.
 */
static int lostfound_dir_link(struct btree_trans *trans, u64 dir_inum, u32 snapshot)
{
	struct bch_inode_unpacked dir;
	try(bch2_inode_find_by_inum_snapshot(trans, dir_inum, snapshot, &dir, 0));

	dir.bi_nlink++;
	dir.bi_snapshot = snapshot;
	return __bch2_fsck_write_inode(trans, &dir);
}

/*
 * @snapshot needs a lost+found and hasn't got one. There's one per snapshot
 * tree, in the tree's root snapshot so that every branch inherits the same one,
 * so either the tree hasn't got one at all or it has and it was deleted here.
 */
static int create_or_restore_lostfound(struct btree_trans *trans, u32 snapshot_tree,
				       u32 snapshot,
				       subvol_inum root_inum,
				       struct bch_inode_unpacked *root_inode,
				       struct bch_hash_info root_hash_info,
				       struct bch_inode_unpacked *lostfound)
{
	struct bch_fs *c = trans->c;

	struct bch_snapshot_tree st;
	try(bch2_snapshot_tree_lookup(trans, snapshot_tree, &st));

	u32 root_snapshot;
	if (bch2_snapshot_live_descendent(c, le32_to_cpu(st.root_snapshot), &root_snapshot) ||
	    !root_snapshot) {
		bch_err(c, "snapshot tree %u has no live snapshot, cannot create lost+found",
			snapshot_tree);
		return bch_err_throw(c, ENOENT_snapshot);
	}

	/*
	 * Keeping lost+found in the root snapshot only gives every branch the
	 * same one if they all inherit from it. If this snapshot doesn't, we
	 * can neither find what's there nor create something it will see.
	 */
	if (!bch2_snapshot_is_ancestor(trans, snapshot, root_snapshot)) {
		bch_err(c, "lost+found for snapshot %u belongs in snapshot %u, which it does not inherit from"
			" (snapshot tree %u, root snapshot %u)",
			snapshot, root_snapshot, snapshot_tree, le32_to_cpu(st.root_snapshot));
		return bch_err_throw(c, snapshot_lostfound_unreachable);
	}

	/*
	 * root_hash_info came from the root inode as @snapshot sees it, and
	 * we're about to hash with it in another snapshot: fine, because all
	 * versions of an inode must have the same hash seed and type, and
	 * bch2_check_dirents has already run and repaired any that didn't
	 * (check_inode_hash_info_matches_root()).
	 */
	u64 inum = 0;
	unsigned d_type = 0;
	u32 dirent_snapshot = 0;
	int ret = lookup_dirent_in_snapshot(trans, root_hash_info, root_inum,
					    &lostfound_str, &inum, &d_type, root_snapshot);
	if (!ret) {
		dirent_snapshot = snapshot;
		ret = restore_lostfound(trans, snapshot, root_snapshot, root_inum,
					root_inode, inum, d_type, lostfound);
	} else if (bch2_err_matches(ret, ENOENT)) {
		dirent_snapshot = root_snapshot;
		ret = create_lostfound(trans, root_snapshot, root_inum, root_inode, lostfound);
	}
	if (ret)
		return ret;

	try(lostfound_dir_link(trans, root_inum.inum, dirent_snapshot));

	return bch2_trans_commit_lazy(trans, NULL, NULL, BCH_TRANS_COMMIT_no_enospc);
}

/* Get lost+found, create if it doesn't exist: */
static int lookup_lostfound(struct btree_trans *trans, u32 snapshot,
			    struct bch_inode_unpacked *lostfound,
			    u64 reattaching_inum)
{
	struct bch_fs *c = trans->c;
	u32 snapshot_tree = bch2_snapshot_tree(c, snapshot);
	int ret;

	u32 subvolid = 0;
	ret = find_snapshot_tree_subvol(trans, snapshot_tree, &subvolid);
	bch_err_msg(c, ret, "finding subvol associated with snapshot tree %u",
		    bch2_snapshot_tree(c, snapshot));
	if (ret)
		return ret;

	struct bkey_i_subvolume subvol;
	ret = bch2_subvolume_get_key(trans, subvolid, false, &subvol);
	bch_err_msg(c, ret, "looking up subvol %u for snapshot %u", subvolid, snapshot);
	if (ret)
		return ret;

	if (!subvol.v.inode) {
		struct bkey_i_subvolume *subvol = errptr_try(bch2_bkey_get_mut_typed(trans,
				BTREE_ID_subvolumes, POS(0, subvolid),
				0, subvolume));

		subvol->v.inode = cpu_to_le64(reattaching_inum);
	}

	subvol_inum root_inum = {
		.subvol = subvolid,
		.inum = le64_to_cpu(subvol.v.inode)
	};

	struct bch_inode_unpacked root_inode;
	ret = bch2_inode_find_by_inum_snapshot(trans, root_inum.inum, snapshot, &root_inode, 0);
	if (ret) {
		/*
		 * The inum came out of the subvolume key, so print the key:
		 * which snapshot it points at is what says whether the root
		 * inode is missing or we're looking in the wrong place.
		 */
		CLASS(printbuf, buf)();

		bch2_bkey_val_to_text(&buf, c, bkey_i_to_s_c(&subvol.k_i));
		bch_err_msg(c, ret, "looking up root inode %llu in snapshot %u, from\n  %s",
			    root_inum.inum, snapshot, buf.buf);
		return ret;
	}

	struct bch_hash_info root_hash_info;
	try(bch2_hash_info_init(c, &root_inode, &root_hash_info));

	u64 inum = 0;
	unsigned d_type = 0;
	ret = lookup_dirent_in_snapshot(trans, root_hash_info, root_inum,
			      &lostfound_str, &inum, &d_type, snapshot);
	if (bch2_err_matches(ret, ENOENT)) {
		/*
		 * We always create lost_found in its own transaction; this will
		 * return a transaction restart:
		 */
		ret = create_or_restore_lostfound(trans, snapshot_tree, snapshot, root_inum,
						  &root_inode, root_hash_info, lostfound);
		bch_err_msg(c, ret, "getting lost+found for snapshot %u", snapshot);
		return ret;
	}

	bch_err_fn(c, ret);
	if (ret)
		return ret;

	if (d_type != DT_DIR) {
		ret = bch_err_throw(c, ENOENT_not_directory);
		bch_err_msg(c, ret, "looking up lost+found");
		return ret;
	}

	/*
	 * The bch2_check_dirents pass has already run, dangling dirents
	 * shouldn't exist here:
	 */
	ret = bch2_inode_find_by_inum_snapshot(trans, inum, snapshot, lostfound, 0);
	bch_err_msg(c, ret, "looking up lost+found %llu:%u in (root inode %llu, snapshot root %u)",
		    inum, snapshot, root_inum.inum, bch2_snapshot_root(c, snapshot));
	return ret;
}

static inline bool inode_should_reattach(struct bch_inode_unpacked *inode)
{
	if (inode->bi_inum == BCACHEFS_ROOT_INO &&
	    inode->bi_subvol == BCACHEFS_ROOT_SUBVOL)
		return false;

	/*
	 * Subvolume roots are special: older versions of subvolume roots may be
	 * disconnected, it's only the newest version that matters.
	 *
	 * We only keep a single dirent pointing to a subvolume root, i.e.
	 * older versions of snapshots will not have a different dirent pointing
	 * to the same subvolume root.
	 *
	 * This is because dirents that point to subvolumes are only visible in
	 * the parent subvolume - versioning is not needed - and keeping them
	 * around would break fsck, because when we're crossing subvolumes we
	 * don't have a consistent snapshot ID to do check the inode <-> dirent
	 * relationships.
	 *
	 * Thus, a subvolume root that's been renamed after a snapshot will have
	 * a disconnected older version - that's expected.
	 *
	 * Note that taking a snapshot always updates the root inode (to update
	 * the dirent backpointer), so a subvolume root inode with
	 * BCH_INODE_has_child_snapshot is never visible.
	 */
	if (inode->bi_subvol &&
	    (inode->bi_flags & BCH_INODE_has_child_snapshot))
		return false;

	return !bch2_inode_has_backpointer(inode) &&
		!(inode->bi_flags & BCH_INODE_unlinked);
}

static int maybe_delete_dirent(struct btree_trans *trans, struct bpos d_pos, u32 snapshot)
{
	CLASS(btree_iter, iter)(trans, BTREE_ID_dirents,
				SPOS(d_pos.inode, d_pos.offset, snapshot),
				BTREE_ITER_intent);
	struct bkey_s_c k = bkey_try(bch2_btree_iter_peek_slot(&iter));

	if (bpos_eq(k.k->p, d_pos)) {
		/*
		 * delete_at() doesn't work because the update path doesn't
		 * internally use BTREE_ITER_with_updates yet
		 *
		 * XXX not true anymore
		 */
		struct bkey_i *k = errptr_try(bch2_trans_kmalloc(trans, sizeof(*k)));

		bkey_init(&k->k);
		k->k.type = KEY_TYPE_whiteout;
		k->k.p = iter.pos;
		return bch2_trans_update(trans, &iter, k, BTREE_UPDATE_internal_snapshot_node);
	}

	return 0;
}

int bch2_reattach_inode(struct btree_trans *trans, struct bch_inode_unpacked *inode)
{
	struct bch_fs *c = trans->c;
	struct bch_inode_unpacked lostfound;
	char name_buf[20];
	int ret;

	u32 dirent_snapshot = inode->bi_snapshot;
	if (inode->bi_subvol) {
		inode->bi_parent_subvol = BCACHEFS_ROOT_SUBVOL;

		struct bkey_i_subvolume *subvol =
			errptr_try(bch2_bkey_get_mut_typed(trans,
						BTREE_ID_subvolumes, POS(0, inode->bi_subvol),
						0, subvolume));

		subvol->v.fs_path_parent = BCACHEFS_ROOT_SUBVOL;

		try(bch2_subvolume_get_snapshot(trans, inode->bi_parent_subvol, &dirent_snapshot));

		snprintf(name_buf, sizeof(name_buf), "subvol-%u", inode->bi_subvol);
	} else {
		snprintf(name_buf, sizeof(name_buf), "%llu", inode->bi_inum);
	}

	try(lookup_lostfound(trans, dirent_snapshot, &lostfound, inode->bi_inum));

	bch_verbose(c, "got lostfound inum %llu", lostfound.bi_inum);

	struct qstr name = QSTR(name_buf);

	/*
	 * Adopt instead of create: the child fixup loop below commits in
	 * chunks (bch2_trans_commit_lazy_if_full()), so a re-drive can find
	 * the reattach dirent already committed - at our snapshot or an
	 * ancestor, when the committed fixups moved the oldest-needing-
	 * reattach point down. The name is deterministic, so look it up:
	 * adopting avoids the STR_HASH_must_create collision and re-bumping
	 * lost+found's nlink.
	 */
	struct bch_hash_info lostfound_hash;
	try(bch2_hash_info_init(c, &lostfound, &lostfound_hash));

	bool adopted = false;
	{
		CLASS(btree_iter_uninit, d_iter)(trans);
		struct bkey_s_c k = bch2_hash_lookup_in_snapshot(trans, &d_iter,
				bch2_dirent_hash_desc, &lostfound_hash,
				(subvol_inum) { inode->bi_parent_subvol, lostfound.bi_inum },
				&name, 0, dirent_snapshot);
		ret = bkey_err(k);
		if (ret && !bch2_err_matches(ret, ENOENT))
			return ret;

		if (!ret) {
			struct bkey_s_c_dirent d = bkey_s_c_to_dirent(k);
			u64 target = d.v->d_type == DT_SUBVOL
				? le32_to_cpu(d.v->d_child_subvol)
				: le64_to_cpu(d.v->d_inum);

			if (target != (inode->bi_subvol ?: inode->bi_inum)) {
				CLASS(printbuf, buf)();
				bch2_bkey_val_to_text(&buf, c, k);
				bch_err(c, "reattaching inode %llu:%u: lost+found entry %s exists but points elsewhere:\n%s",
					inode->bi_inum, inode->bi_snapshot, name_buf, buf.buf);
				return bch_err_throw(c, fsck_repair_unimplemented);
			}

			inode->bi_dir		= lostfound.bi_inum;
			inode->bi_dir_offset	= d.k->p.offset;
			adopted = true;
		}
	}

	/*
	 * is_subdir_for_nlink(), not S_ISDIR(): a subvolume root is named by a
	 * DT_SUBVOL dirent, which doesn't count towards its parent's link
	 * count. Bumping it here for one leaves check_nlinks() to disagree.
	 */
	if (!adopted)
		lostfound.bi_nlink += is_subdir_for_nlink(inode);

	/*
	 * Ensure lost+found has an inode version in the snapshot we're about to
	 * create the dirent in, or we leave a key in a snapshot whose inode only
	 * exists in an ancestor - snapshot_key_missing_inode_snapshot, which the
	 * next check_dirents has to clean up after us.
	 *
	 * dirent_snapshot is the inode's own snapshot for an ordinary inode, and
	 * the parent subvolume's for a subvolume root (above); lookup_lostfound()
	 * resolved lost+found from it, so it is at worst an ancestor of it.
	 */
	BUG_ON(!bch2_snapshot_is_ancestor(trans, dirent_snapshot, lostfound.bi_snapshot));
	lostfound.bi_snapshot = dirent_snapshot;

	try(__bch2_fsck_write_inode(trans, &lostfound));

	if (!adopted) {
		inode->bi_dir = lostfound.bi_inum;

		ret = bch2_dirent_create_snapshot(trans,
					inode->bi_parent_subvol,
					dirent_snapshot,
					&lostfound,
					inode_d_type(inode),
					&name,
					inode->bi_subvol ?: inode->bi_inum,
					&inode->bi_dir_offset,
					BTREE_UPDATE_internal_snapshot_node|
					STR_HASH_must_create);
		if (ret) {
			bch_err_msg(c, ret, "error creating dirent");
			return ret;
		}
	}

	try(__bch2_fsck_write_inode(trans, inode));

	{
		CLASS(printbuf, buf)();
		try(bch2_inum_snapshot_to_path(trans, inode->bi_inum,
					       inode->bi_snapshot, NULL, &buf));

		if (adopted)
			bch_verbose(c, "resuming reattach at %s", buf.buf);
		else
			bch_info(c, "reattached at %s", buf.buf);
	}

	/*
	 * Fix up inodes in child snapshots: if they should also be reattached
	 * update the backpointer field, if they should not be we need to emit
	 * whiteouts for the dirent we just created.
	 */
	if (!inode->bi_subvol && bch2_snapshot_is_leaf(c, inode->bi_snapshot) <= 0) {
		CLASS(snapshot_id_list, whiteouts_done)();
		struct bkey_s_c k;

		darray_init(&whiteouts_done);

		for_each_btree_key_reverse_norestart(trans, iter,
				BTREE_ID_inodes, SPOS(0, inode->bi_inum, inode->bi_snapshot - 1),
				BTREE_ITER_all_snapshots|BTREE_ITER_intent, k, ret) {
			if (k.k->p.offset != inode->bi_inum)
				break;

			/*
			 * This loop batches an update per descendant snapshot
			 * version into one transaction; a fat chain overflows
			 * the bump allocator. Commit once substantial work has
			 * accumulated - the restart re-drives us, the adopt
			 * path above resumes without duplicating the reattach
			 * dirent, and already-fixed children are skipped
			 * below:
			 */
			try(bch2_trans_commit_lazy_if_full(trans, NULL, NULL,
					BCH_TRANS_COMMIT_no_enospc));

			if (!bkey_is_inode(k.k) ||
			    !bch2_snapshot_is_ancestor(trans, k.k->p.snapshot, inode->bi_snapshot) ||
			    snapshot_list_has_ancestor(trans, &whiteouts_done, k.k->p.snapshot))
				continue;

			struct bch_inode_unpacked child_inode;
			bch2_inode_unpack(c, k, &child_inode);

			/*
			 * Fixed by a previous partial commit: its backpointer
			 * already names our reattach dirent. Must be checked
			 * before inode_should_reattach() - having a
			 * backpointer, it would fall into the whiteout arm
			 * and turn the committed fixup into a dangling
			 * backpointer:
			 */
			if (child_inode.bi_dir == inode->bi_dir &&
			    child_inode.bi_dir_offset == inode->bi_dir_offset)
				continue;

			if (!inode_should_reattach(&child_inode)) {
				try(maybe_delete_dirent(trans,
							SPOS(lostfound.bi_inum, inode->bi_dir_offset,
							     dirent_snapshot),
							k.k->p.snapshot));
				try(snapshot_list_add(c, &whiteouts_done, k.k->p.snapshot));
			} else {
				iter.snapshot = k.k->p.snapshot;
				child_inode.bi_dir = inode->bi_dir;
				child_inode.bi_dir_offset = inode->bi_dir_offset;

				try(bch2_inode_write_flags(trans, &iter, &child_inode,
							   BTREE_UPDATE_internal_snapshot_node));
			}
		}
	}

	return ret;
}

int bch2_reconstruct_subvol(struct btree_trans *trans, u32 snapshotid, u32 subvolid, u64 inum)
{
	struct bch_fs *c = trans->c;

	if (!bch2_snapshot_is_leaf(c, snapshotid)) {
		bch_err(c, "need to reconstruct subvol, but have interior node snapshot");
		return bch_err_throw(c, fsck_repair_unimplemented);
	}

	/*
	 * Without an inum from the caller, find the root inode rather than
	 * minting one: the inode carrying bi_subvol == subvolid is the root,
	 * and when it's the subvolume key that went missing that inode is
	 * still there. Creating a second one would leave two claimants for the
	 * same subvolume and the real contents orphaned behind the new empty
	 * root.
	 *
	 * It can't be deferred to a later pass either - bch2_subvolume_validate()
	 * rejects a subvolume key with inode == 0 (subvol_inode_bad), so the
	 * key can't be written at all until we know it.
	 */
	if (!inum) {
		struct bkey_s_c k;
		int ret = 0;

		for_each_btree_key_norestart(trans, iter, BTREE_ID_inodes, POS_MIN,
					     BTREE_ITER_prefetch|BTREE_ITER_all_snapshots, k, ret) {
			if (!bkey_is_inode(k.k))
				continue;

			struct bch_inode_unpacked candidate;
			bch2_inode_unpack(c, k, &candidate);

			if (candidate.bi_subvol == subvolid) {
				inum = candidate.bi_inum;
				break;
			}
		}
		if (ret)
			return ret;

		if (!inum) {
			bch_err(c, "no root inode found for subvol %u, can't reconstruct",
				subvolid);
			return bch_err_throw(c, fsck_repair_unimplemented);
		}
	}

	bch_info(c, "reconstructing subvol %u with root inode %llu", subvolid, inum);

	struct bkey_i_subvolume *new_subvol = errptr_try(bch2_trans_kmalloc(trans, sizeof(*new_subvol)));

	bkey_subvolume_init(&new_subvol->k_i);
	new_subvol->k.p.offset	= subvolid;
	new_subvol->v.snapshot	= cpu_to_le32(snapshotid);
	new_subvol->v.inode	= cpu_to_le64(inum);
	bch2_subvolume_state_set(&new_subvol->v, SUBVOLUME_STATE_live);
	try(bch2_btree_insert_trans(trans, BTREE_ID_subvolumes, &new_subvol->k_i, 0));

	struct bkey_i_snapshot *s = bch2_bkey_get_mut_typed(trans,
			BTREE_ID_snapshots, POS(0, snapshotid),
			0, snapshot);
	int ret = PTR_ERR_OR_ZERO(s);
	bch_err_msg(c, ret, "getting snapshot %u", snapshotid);
	if (ret)
		return ret;

	u32 snapshot_tree = le32_to_cpu(s->v.tree);

	s->v.subvol = cpu_to_le32(subvolid);
	bch2_snapshot_state_set(&s->v, SNAPSHOT_STATE_live);

	struct bkey_i_snapshot_tree *st = bch2_bkey_get_mut_typed(trans,
			BTREE_ID_snapshot_trees, POS(0, snapshot_tree),
			0, snapshot_tree);
	ret = PTR_ERR_OR_ZERO(st);
	bch_err_msg(c, ret, "getting snapshot tree %u", snapshot_tree);
	if (ret)
		return ret;

	if (!st->v.master_subvol)
		st->v.master_subvol = cpu_to_le32(subvolid);
	return 0;
}

static int reconstruct_inode(struct btree_trans *trans, enum btree_id btree, u32 snapshot, u64 inum)
{
	struct bch_fs *c = trans->c;
	unsigned i_mode = S_IFREG;
	u64 i_size = 0;

	switch (btree) {
	case BTREE_ID_extents: {
		CLASS(btree_iter, iter)(trans, BTREE_ID_extents, SPOS(inum, U64_MAX, snapshot), 0);
		struct bkey_s_c k = bkey_try(bch2_btree_iter_peek_prev_min(&iter, POS(inum, 0)));

		/* may race with repair deleting the extents that triggered us: */
		if (k.k)
			i_size = k.k->p.offset << 9;
		break;
	}
	case BTREE_ID_dirents:
		i_mode = S_IFDIR;
		break;
	case BTREE_ID_xattrs:
		break;
	default:
		BUG();
	}

	struct bch_inode_unpacked new_inode;
	bch2_inode_init_early(c, &new_inode);
	bch2_inode_init_late(c, &new_inode, bch2_current_time(c), 0, 0, i_mode|0600, 0, NULL);
	new_inode.bi_size = i_size;
	new_inode.bi_inum = inum;
	new_inode.bi_snapshot = snapshot;

	/*
	 * Recover the hash info if any version of this inode survives anywhere.
	 *
	 * bi_hash_seed and the str_hash type are the same in every snapshot
	 * version of an inode - bch2_repair_inode_hash_info() exists to enforce
	 * that - so a descendant will do when no ancestor is left. Btree node
	 * loss takes out one snapshot's inode key while leaving another's, and
	 * an ancestor-only search calls that unrecoverable and falls back to the
	 * random seed bch2_inode_init_early() left in new_inode. That puts every
	 * dirent already under this directory at the wrong hash offset: lookups
	 * miss, so creates insert duplicates instead of overwriting, and the
	 * directory quietly becomes untraversable.
	 */
	struct bch_inode_unpacked hash_src;
	int ret = bch2_inode_find_oldest_snapshot(trans, inum, snapshot, &hash_src);
	if (bch2_err_matches(ret, ENOENT))
		ret = bch2_inode_find_any_snapshot(trans, inum, &hash_src);
	if (ret && !bch2_err_matches(ret, ENOENT))
		return ret;
	if (!ret) {
		new_inode.bi_hash_seed = hash_src.bi_hash_seed;
		SET_INODE_STR_HASH(&new_inode, INODE_STR_HASH(&hash_src));
	}

	return __bch2_fsck_write_inode(trans, &new_inode);
}

int bch2_snapshots_seen_update(struct bch_fs *c, struct snapshots_seen *s,
			       enum btree_id btree_id, struct bpos pos)
{
	if (!bkey_eq(s->pos, pos))
		s->ids.nr = 0;
	s->pos = pos;

	return snapshot_list_add_nodup(c, &s->ids, pos.snapshot);
}

/**
 * bch2_key_visible_in_snapshot - returns true if @id is a descendent of @ancestor,
 * and @ancestor hasn't been overwritten in @seen
 *
 * @c:		filesystem handle
 * @seen:	list of snapshot ids already seen at current position
 * @id:		descendent snapshot id
 * @ancestor:	ancestor snapshot id
 *
 * Returns:	whether key in @ancestor snapshot is visible in @id snapshot
 */
bool bch2_key_visible_in_snapshot(struct btree_trans *trans, struct snapshots_seen *seen,
				  u32 id, u32 ancestor)
{
	EBUG_ON(id > ancestor);

	if (id == ancestor)
		return true;

	if (!bch2_snapshot_is_ancestor(trans, id, ancestor))
		return false;

	/*
	 * We know that @id is a descendant of @ancestor, we're checking if
	 * we've seen a key that overwrote @ancestor - i.e. also a descendent of
	 * @ascestor and with @id as a descendent.
	 *
	 * But we already know that we're scanning IDs between @id and @ancestor
	 * numerically, since snapshot ID lists are kept sorted, so if we find
	 * an id that's an ancestor of @id we're done:
	 */
	darray_for_each_reverse(seen->ids, i)
		if (*i != ancestor && bch2_snapshot_is_ancestor(trans, id, *i))
			return false;

	return true;
}

/**
 * bch2_ref_visible - given a key with snapshot id @src that points to a key with
 * snapshot id @dst, test whether there is some snapshot in which @dst is
 * visible.
 *
 * @c:		filesystem handle
 * @s:		list of snapshot IDs already seen at @src
 * @src:	snapshot ID of src key
 * @dst:	snapshot ID of dst key
 * Returns:	true if there is some snapshot in which @dst is visible
 *
 * Assumes we're visiting @src keys in natural key order
 */
bool bch2_ref_visible(struct btree_trans *trans, struct snapshots_seen *s, u32 src, u32 dst)
{
	return dst <= src
		? bch2_key_visible_in_snapshot(trans, s, dst, src)
		: bch2_snapshot_is_ancestor(trans, src, dst);
}

int bch2_ref_visible2(struct btree_trans *trans,
		      u32 src, struct snapshots_seen *src_seen,
		      u32 dst, struct snapshots_seen *dst_seen)
{
	if (dst > src) {
		swap(dst, src);
		swap(dst_seen, src_seen);
	}
	return bch2_key_visible_in_snapshot(trans, src_seen, dst, src);
}

#define for_each_visible_inode(_trans, _s, _w, _snapshot, _i)				\
	for (_i = (_w)->inodes.data; _i < (_w)->inodes.data + (_w)->inodes.nr &&	\
	     (_i)->inode.bi_snapshot <= (_snapshot); _i++)				\
		if (bch2_key_visible_in_snapshot(_trans, _s, _i->inode.bi_snapshot, _snapshot))

static int add_inode(struct bch_fs *c, struct inode_walker *w,
		     struct bkey_s_c inode)
{
	try(darray_push(&w->inodes, ((struct inode_walker_entry) {
		.whiteout	= !bkey_is_inode(inode.k),
	})));

	struct inode_walker_entry *n = &darray_last(w->inodes);
	if (!n->whiteout) {
		bch2_inode_unpack(c, inode, &n->inode);
	} else {
		n->inode.bi_inum	= inode.k->p.offset;
		n->inode.bi_snapshot	= inode.k->p.snapshot;
	}
	return 0;
}

static int get_inodes_all_snapshots(struct btree_trans *trans,
				    struct inode_walker *w, u64 inum)
{
	struct bch_fs *c = trans->c;
	struct bkey_s_c k;
	int ret;

	/*
	 * We no longer have inodes for w->last_pos; clear this to avoid
	 * screwing up check_i_sectors/check_subdir_count if we take a
	 * transaction restart here:
	 */
	w->have_inodes = false;
	w->recalculate_sums = false;
	w->inodes.nr = 0;

	for_each_btree_key_max_norestart(trans, iter,
			BTREE_ID_inodes, POS(0, inum), SPOS(0, inum, U32_MAX),
			BTREE_ITER_all_snapshots, k, ret)
		try(add_inode(c, w, k));

	if (ret)
		return ret;

	w->first_this_inode = true;
	w->have_inodes = true;
	w->commit_count = trans->commit_count;
	return 0;
}

static int get_visible_inodes(struct btree_trans *trans,
			      struct inode_walker *w,
			      struct snapshots_seen *s,
			      u64 inum)
{
	struct bch_fs *c = trans->c;
	struct bkey_s_c k;
	int ret;

	w->inodes.nr = 0;
	w->deletes.nr = 0;

	for_each_btree_key_reverse_norestart(trans, iter, BTREE_ID_inodes, SPOS(0, inum, s->pos.snapshot),
			   BTREE_ITER_all_snapshots, k, ret) {
		if (k.k->p.offset != inum)
			break;

		if (!bch2_ref_visible(trans, s, s->pos.snapshot, k.k->p.snapshot))
			continue;

		if (snapshot_list_has_ancestor(trans, &w->deletes, k.k->p.snapshot))
			continue;

		ret = bkey_is_inode(k.k)
			? add_inode(c, w, k)
			: snapshot_list_add(c, &w->deletes, k.k->p.snapshot);
		if (ret)
			break;
	}

	return ret;
}

static struct inode_walker_entry *
lookup_inode_for_snapshot(struct btree_trans *trans, struct inode_walker *w, struct bkey_s_c k)
{
	struct bch_fs *c = trans->c;

	u32 k_snapshot = bch2_snapshot_redundant_interior(c, k.k->p.snapshot) ?: k.k->p.snapshot;

	struct inode_walker_entry *i = darray_find_p(w->inodes, i,
		    bch2_snapshot_is_ancestor(trans, k_snapshot, i->inode.bi_snapshot));

	if (!i)
		return NULL;

	CLASS(printbuf, buf)();
	int ret = 0;

	u32 inode_snapshot = bch2_snapshot_redundant_interior(c, i->inode.bi_snapshot) ?: i->inode.bi_snapshot;

	if (fsck_err_on(k_snapshot != inode_snapshot,
			trans, snapshot_key_missing_inode_snapshot,
			 "have key for inode %llu:%u but have inode in ancestor snapshot %u\n"
			 "unexpected because we should always update the inode when we update a key in that inode\n"
			 "%s",
			 w->last_pos.inode, k.k->p.snapshot, i->inode.bi_snapshot,
			 (bch2_bkey_val_to_text(&buf, c, k),
			  buf.buf))) {
		if (!i->whiteout) {
			struct bch_inode_unpacked new = i->inode;
			new.bi_snapshot = k.k->p.snapshot;
			ret = __bch2_fsck_write_inode(trans, &new);
		} else {
			struct bkey_i whiteout;
			bkey_init(&whiteout.k);
			whiteout.k.type = KEY_TYPE_whiteout;
			whiteout.k.p = SPOS(0, i->inode.bi_inum, k.k->p.snapshot);
			ret = bch2_btree_insert_trans(trans, BTREE_ID_inodes,
						      &whiteout,
						      BTREE_ITER_cached|
						      BTREE_UPDATE_internal_snapshot_node);
		}

		if (ret)
			return ERR_PTR(ret);

		ret = bch2_trans_commit(trans, NULL, NULL, 0);
		if (ret)
			return ERR_PTR(ret);

		struct inode_walker_entry new_entry = *i;

		new_entry.inode.bi_snapshot	= k.k->p.snapshot;
		new_entry.count			= 0;

		while (i > w->inodes.data && i[-1].inode.bi_snapshot > k.k->p.snapshot)
			--i;

		size_t pos = i - w->inodes.data;
		ret = darray_insert_item(&w->inodes, pos, new_entry);
		if (ret)
			return ERR_PTR(ret);

		return ERR_PTR(btree_trans_restart(trans, BCH_ERR_transaction_restart_nested));
	}

	return i;
fsck_err:
	return ERR_PTR(ret);
}

struct inode_walker_entry *bch2_walk_inode(struct btree_trans *trans,
					   struct inode_walker *w,
					   struct bkey_s_c k)
{
	if (w->last_pos.inode != k.k->p.inode) {
		int ret = get_inodes_all_snapshots(trans, w, k.k->p.inode);
		if (ret)
			return ERR_PTR(ret);
	} else if (w->commit_count != trans->commit_count) {
		/*
		 * A commit may have updated inodes we have cached: revalidate.
		 * We're mid way through walking this inode's keys, so per-inode
		 * accumulations (i_sectors, subdir counts) are now partial -
		 * recount instead of complaining:
		 */
		int ret = get_inodes_all_snapshots(trans, w, k.k->p.inode);
		if (ret)
			return ERR_PTR(ret);
		w->recalculate_sums = true;
	}

	w->last_pos = k.k->p;

	return lookup_inode_for_snapshot(trans, w, k);
}

/*
 * Prefer to delete the first one, since that will be the one at the wrong
 * offset:
 * return value: 0 -> delete k1, 1 -> delete k2
 */
int bch2_fsck_update_backpointers(struct btree_trans *trans,
				  struct snapshots_seen *s,
				  const struct bch_hash_desc desc,
				  struct bch_hash_info *hash_info,
				  struct bkey_i *new)
{
	if (new->k.type != KEY_TYPE_dirent)
		return 0;

	struct bkey_i_dirent *d = bkey_i_to_dirent(new);
	CLASS(inode_walker, target)();

	if (d->v.d_type == DT_SUBVOL) {
		/*
		 * A subvolume dirent's backpointer lives on the child
		 * subvolume's root inode (bi_dir_offset), same as a regular
		 * inode - see bch2_inode_get_dirent(). Resolve child_subvol -> root
		 * inode and update it. A dangling subvol dirent (subvol or root
		 * inode gone) is check_subvols/check_dirents' problem, not ours.
		 */
		struct bch_subvolume subvol;
		int ret = bch2_subvolume_get(trans, le32_to_cpu(d->v.d_child_subvol),
					     false, &subvol);
		if (bch2_err_matches(ret, ENOENT))
			return 0;
		if (ret)
			return ret;

		struct bch_inode_unpacked root_inode;
		ret = bch2_inode_find_by_inum_snapshot(trans, le64_to_cpu(subvol.inode),
						       le32_to_cpu(subvol.snapshot),
						       &root_inode, 0);
		if (bch2_err_matches(ret, ENOENT))
			return 0;
		if (ret)
			return ret;

		if (root_inode.bi_dir == d->k.p.inode &&
		    root_inode.bi_dir_offset == d->k.p.offset)
			return 0;

		root_inode.bi_dir		= d->k.p.inode;
		root_inode.bi_dir_offset	= d->k.p.offset;
		return __bch2_fsck_write_inode(trans, &root_inode);
	} else {
		try(get_visible_inodes(trans, &target, s, le64_to_cpu(d->v.d_inum)));

		/*
		 * A backpointer is the (bi_dir, bi_dir_offset) pair - compare
		 * and set both, or an offset match into the wrong directory
		 * skips a broken backpointer, and an offset-only write
		 * manufactures one.
		 *
		 * Skip before the write: __bch2_fsck_write_inode allocates a
		 * bkey_inode_buf of trans mem per call, and this loop runs once
		 * per visible snapshot version in one transaction - an
		 * already-correct backpointer must cost nothing, both to bound
		 * trans mem and so a re-run over partially-repaired state
		 * shrinks instead of repeating the whole batch.
		 */
		darray_for_each(target.inodes, i) {
			if (i->inode.bi_dir == d->k.p.inode &&
			    i->inode.bi_dir_offset == d->k.p.offset)
				continue;

			i->inode.bi_dir		= d->k.p.inode;
			i->inode.bi_dir_offset	= d->k.p.offset;
			try(__bch2_fsck_write_inode(trans, &i->inode));
		}

		return 0;
	}
}

static int check_inode_deleted_list(struct btree_trans *trans, struct bpos p)
{
	CLASS(btree_iter, iter)(trans, BTREE_ID_deleted_inodes, p, 0);
	struct bkey_s_c k = bch2_btree_iter_peek_slot(&iter);
	return bkey_err(k) ?: k.k->type == KEY_TYPE_set;
}

static int check_inode_dirent_inode(struct btree_trans *trans,
				    struct bch_inode_unpacked *inode,
				    bool *write_inode)
{
	struct bch_fs *c = trans->c;
	CLASS(printbuf, buf)();

	u32 inode_snapshot = inode->bi_snapshot;
	CLASS(btree_iter_uninit, dirent_iter)(trans);
	struct bkey_s_c_dirent d = bch2_inode_get_dirent(trans, &dirent_iter, inode, &inode_snapshot);
	int ret = bkey_err(d);
	if (ret && !bch2_err_matches(ret, ENOENT))
		return ret;

	if ((ret || dirent_points_to_inode_nowarn(c, d, inode)) &&
	    inode->bi_subvol &&
	    (inode->bi_flags & BCH_INODE_has_child_snapshot)) {
		/* Older version of a renamed subvolume root: we won't have a
		 * correct dirent for it. That's expected, see
		 * inode_should_reattach().
		 *
		 * We don't clear the backpointer field when doing the rename
		 * because there might be arbitrarily many versions in older
		 * snapshots.
		 */
		inode->bi_dir = 0;
		inode->bi_dir_offset = 0;
		*write_inode = true;
		return 0;
	}

	if (inode_fsck_err_on(ret,
			trans, SPOS(0, inode->bi_inum, inode->bi_snapshot),
			inode_points_to_missing_dirent,
			"inode points to missing dirent\n%s",
			(bch2_inode_unpacked_to_text(&buf, inode), buf.buf)) ||
	    inode_fsck_err_on(!ret && dirent_points_to_inode_nowarn(c, d, inode),
			trans, SPOS(0, inode->bi_inum, inode->bi_snapshot),
			inode_points_to_wrong_dirent,
			"%s",
			(printbuf_reset(&buf),
			 bch2_dirent_inode_mismatch_msg(&buf, c, d, inode),
			 buf.buf))) {
		/*
		 * We just clear the backpointer fields for now. If we find a
		 * dirent that points to this inode in check_dirents(), we'll
		 * update it then; then when we get to check_path() if the
		 * backpointer is still 0 we'll reattach it.
		 */
		inode->bi_dir = 0;
		inode->bi_dir_offset = 0;
		*write_inode = true;
	}

	if (!ret &&
	    !dirent_points_to_inode_nowarn(c, d, inode) &&
	    fsck_err_on(inode->bi_flags & BCH_INODE_unlinked,
			trans, inode_unlinked_but_has_dirent,
			"inode unlinked but has dirent\n%s",
			(printbuf_reset(&buf),
			 bch2_inode_unpacked_to_text(&buf, inode),
			 prt_newline(&buf),
			 bch2_bkey_val_to_text(&buf, c, d.s_c),
			 buf.buf))) {
		/*
		 * The dirent was just verified to point at this inode, so the
		 * unlinked flag is wrong - and the flag clear is the complete
		 * repair: bi_nlink counts links beyond the first, so this
		 * yields nlink 1 (check_nlinks recounts hardlinks), and the
		 * inode trigger removes the deleted_inodes entry.
		 */
		inode->bi_flags &= ~BCH_INODE_unlinked;
		*write_inode = true;
	}

	ret = 0;
fsck_err:
	bch_err_fn(c, ret);
	return ret;
}

/*
 * Returns 1 if an xattr of the given type exists for @inode, 0 if not,
 * negative on error. Used to verify the BCH_INODE_has_*_acl flags against
 * what's actually in the xattr btree.
 */
static int inode_has_xattr_type(struct btree_trans *trans,
				struct bch_inode_unpacked *inode,
				unsigned xattr_type)
{
	struct bch_hash_info hash;
	try(bch2_hash_info_init(trans->c, inode, &hash));

	struct xattr_search_key search = X_SEARCH(xattr_type, "", 0);
	CLASS(btree_iter_uninit, iter)(trans);
	int ret = bkey_err(bch2_hash_lookup_in_snapshot(
				trans, &iter, bch2_xattr_hash_desc, &hash,
				(subvol_inum) { 0, inode->bi_inum },
				&search, 0, inode->bi_snapshot));
	if (bch2_err_matches(ret, ENOENT))
		return 0;
	return ret ?: 1;
}

static int check_inode(struct btree_trans *trans,
		       struct btree_iter *iter,
		       struct bkey_s_c k,
		       struct bch_inode_unpacked *snapshot_root,
		       struct snapshots_seen *s)
{
	struct bch_fs *c = trans->c;
	CLASS(printbuf, buf)();
	struct bch_inode_unpacked u;
	bool do_update = false;

	int ret = bch2_check_key_has_snapshot(trans, iter, k);
	if (ret)
		return ret < 0 ? ret : 0;

	try(bch2_snapshots_seen_update(c, s, iter->btree_id, k.k->p));

	if (!bkey_is_inode(k.k))
		return 0;

	bch2_inode_unpack(c, k, &u);
	BUG_ON(u.bi_snapshot != k.k->p.snapshot);

	if (snapshot_root->bi_inum != u.bi_inum ||
	    !bch2_snapshot_is_ancestor(trans, u.bi_snapshot, snapshot_root->bi_snapshot))
		try(bch2_inode_find_oldest_snapshot(trans, u.bi_inum, u.bi_snapshot, snapshot_root));

	if (u.bi_hash_seed	!= snapshot_root->bi_hash_seed ||
	    INODE_STR_HASH(&u)	!= INODE_STR_HASH(snapshot_root))
		try(bch2_repair_inode_hash_info(trans, &u, snapshot_root));

	ret = bch2_check_inode_has_case_insensitive(trans, &u, &s->ids, &do_update);
	if (bch2_err_matches(ret, ENOENT)) /* disconnected inode; will be fixed by a later pass */
		ret = 0;
	bch_err_msg(c, ret, "bch2_check_inode_has_case_insensitive()");
	if (ret)
		return ret;

	if (bch2_inode_has_backpointer(&u))
		try(check_inode_dirent_inode(trans, &u, &do_update));

	if (S_ISDIR(u.bi_mode) && (u.bi_flags & BCH_INODE_unlinked)) {
		/*
		 * BCH_INODE_unlinked is allowed on a directory only if it's a
		 * subvolume root and the subvolume is unlinked - the root is
		 * then legitimately non-empty, since the snapshot sweep is
		 * what deletes the contents. Anything else: strip the flag so
		 * check_unreachable_inodes() reattaches it.
		 */
		ret = bch2_inode_is_subvolume_root(&u)
			? bch2_subvolume_is_unlinked(trans, u.bi_subvol)
			: 0;
		if (ret < 0)
			return ret;

		if (!ret) {
			/* Check for this early so that check_unreachable_inode() will reattach it */

			ret = bch2_empty_dir_snapshot(trans, k.k->p.offset, 0, k.k->p.snapshot);
			if (ret && ret != -BCH_ERR_ENOTEMPTY_dir_not_empty)
				return ret;

			inode_fsck_err_on(ret, trans, k.k->p,
				    inode_dir_unlinked_but_not_empty,
				    "dir unlinked but not empty\n%s",
				    (printbuf_reset(&buf),
				     bch2_inode_unpacked_to_text(&buf, &u),
				     buf.buf));
			u.bi_flags &= ~BCH_INODE_unlinked;
			do_update = true;
		}
		ret = 0;
	}

	if (fsck_err_on(S_ISDIR(u.bi_mode) && u.bi_size,
			trans, inode_dir_has_nonzero_i_size,
			"directory with nonzero i_size\n%s",
			(printbuf_reset(&buf),
			 bch2_inode_unpacked_to_text(&buf, &u),
			 buf.buf))) {
		u.bi_size = 0;
		do_update = true;
	}

	ret = bch2_inode_has_child_snapshots(trans, k.k->p);
	if (ret < 0)
		return ret;

	if (fsck_err_on(ret != !!(u.bi_flags & BCH_INODE_has_child_snapshot),
			trans, inode_has_child_snapshots_wrong,
			"inode has_child_snapshots flag wrong (should be %u)\n%s",
			ret,
			(printbuf_reset(&buf),
			 bch2_inode_unpacked_to_text(&buf, &u),
			 buf.buf))) {
		if (ret)
			u.bi_flags |= BCH_INODE_has_child_snapshot;
		else
			u.bi_flags &= ~BCH_INODE_has_child_snapshot;
		do_update = true;
	}
	ret = 0;

	/*
	 * Unlinked subvolume roots are skipped here: their deletion belongs
	 * to the subvolume path (check_subvols() resumes it after a crash),
	 * not the inode reaper or the deleted_inodes btree:
	 */
	if ((u.bi_flags & BCH_INODE_unlinked) &&
	    !bch2_inode_is_subvolume_root(&u) &&
	    !(u.bi_flags & BCH_INODE_has_child_snapshot)) {
		if (!test_bit(BCH_FS_started, &c->flags)) {
			/*
			 * If we're not in online fsck, don't delete unlinked
			 * inodes, just make sure they're on the deleted list.
			 *
			 * They might be referred to by a logged operation -
			 * i.e. we might have crashed in the middle of a
			 * truncate on an unlinked but open file - so we want to
			 * let the delete_dead_inodes kill it after resuming
			 * logged ops.
			 */
			ret = check_inode_deleted_list(trans, k.k->p);
			if (ret < 0)
				return ret;

			fsck_err_on(!ret,
				    trans, unlinked_inode_not_on_deleted_list,
				    "inode unlinked, but not on deleted list\n%s",
				    (printbuf_reset(&buf),
				     bch2_inode_unpacked_to_text(&buf, &u),
				     buf.buf));

			try(bch2_btree_bit_mod_buffered(trans, BTREE_ID_deleted_inodes, k.k->p, 1));
			ret = 0;
		} else {
			ret = bch2_inode_or_descendents_is_open(trans, k.k->p);
			if (ret < 0)
				return ret;

			if (fsck_err_on(!ret,
					trans, inode_unlinked_and_not_open,
				      "inode unlinked and not open\n%s",
				      (printbuf_reset(&buf),
				       bch2_inode_unpacked_to_text(&buf, &u),
				       buf.buf))) {
				ret = bch2_inode_rm_snapshot(trans, u.bi_inum, iter->pos.snapshot);
				bch_err_msg(c, ret, "in fsck deleting inode");
				return ret;
			}
			ret = 0;
		}
	}

	if (fsck_err_on(u.bi_parent_subvol &&
			(u.bi_subvol == 0 ||
			 u.bi_subvol == BCACHEFS_ROOT_SUBVOL),
			trans, inode_bi_parent_nonzero,
			"inode has nonzero bi_parent_subvol but is not a subvolume root\n%s",
			(printbuf_reset(&buf),
			 bch2_inode_unpacked_to_text(&buf, &u),
			 buf.buf))) {
		u.bi_parent_subvol = 0;
		do_update = true;
	}

	bool has_opts = false;
#define x(_name, _bits)  if (u.bi_##_name) has_opts = true;
	BCH_INODE_OPTS()
#undef x
	if (fsck_err_on((bool) (u.bi_flags & BCH_INODE_has_inode_opts) != has_opts,
			trans, inode_has_inode_opts_flag_wrong,
			"inode has_inode_opts flag wrong, should be %u\n%s",
			has_opts,
			(printbuf_reset(&buf),
			 bch2_inode_unpacked_to_text(&buf, &u),
			 buf.buf))) {
		u.bi_flags &= ~BCH_INODE_has_inode_opts;
		if (has_opts)
			u.bi_flags |= BCH_INODE_has_inode_opts;
		do_update = true;
	}

	/* after the flag check above: it's what gates the walk */
	try(bch2_check_inode_opts_propagated(trans, &u));

	/*
	 * has_access_acl/has_default_acl: only the set direction is verified
	 * here, an xattr lookup per flagged inode - inodes with ACLs are
	 * rare. Flag clear but xattr present, the direction that would break
	 * the bch2_get_acl() short circuit, is caught from the xattr side in
	 * check_xattr():
	 */
	if (u.bi_flags & BCH_INODE_has_access_acl) {
		int has = inode_has_xattr_type(trans, &u,
					       KEY_TYPE_XATTR_INDEX_POSIX_ACL_ACCESS);
		if (has < 0)
			return has;
		if (fsck_err_on(!has, trans, inode_has_access_acl_flag_wrong,
				"inode has BCH_INODE_has_access_acl set but no acl xattr\n%s",
				(printbuf_reset(&buf),
				 bch2_inode_unpacked_to_text(&buf, &u),
				 buf.buf))) {
			u.bi_flags &= ~BCH_INODE_has_access_acl;
			do_update = true;
		}
	}

	if (u.bi_flags & BCH_INODE_has_default_acl) {
		int has = inode_has_xattr_type(trans, &u,
					       KEY_TYPE_XATTR_INDEX_POSIX_ACL_DEFAULT);
		if (has < 0)
			return has;
		if (fsck_err_on(!has, trans, inode_has_default_acl_flag_wrong,
				"inode has BCH_INODE_has_default_acl set but no acl xattr\n%s",
				(printbuf_reset(&buf),
				 bch2_inode_unpacked_to_text(&buf, &u),
				 buf.buf))) {
			u.bi_flags &= ~BCH_INODE_has_default_acl;
			do_update = true;
		}
	}

	/*
	 * Not gated on the snapshot being a leaf: taking a snapshot rewrites
	 * the root inode of the new subvolume, not of the old, so a live
	 * subvolume's root inode key stays at a node that has since become
	 * interior. Leaf-ness therefore skipped this block for every subvolume
	 * that had ever been snapshotted - so a lost subvolume key was never
	 * reconstructed and bi_subvol was never validated for exactly the
	 * subvolumes with the most history behind them.
	 *
	 * Live versus stale is decided below instead, by inode_bi_subvol_wrong:
	 * the subvolume's snapshot has to have this key's snapshot as an
	 * ancestor, which is the actual question leaf-ness was standing in for.
	 */
	if (u.bi_subvol) {
		struct bch_subvolume s;

		ret = bch2_subvolume_get(trans, u.bi_subvol, false, &s);
		if (ret && !bch2_err_matches(ret, ENOENT))
			return ret;

		struct bch_snapshot snapshot;
		int snapshot_ret = bch2_snapshot_lookup(trans, u.bi_snapshot, &snapshot);
		if (snapshot_ret && !bch2_err_matches(snapshot_ret, ENOENT))
			return snapshot_ret;

		/*
		 * A missing subvolume is reconstructed in two cases. If the
		 * subvolumes btree is known to have lost data, reconstruct. Or
		 * if this root inode names the subvolume and the live snapshot
		 * it lives at names the same subvolume in its backref,
		 * reconstruct: two keys point at the edge and only the
		 * subvolume key is gone.
		 *
		 * If the snapshot is will_delete, don't - there the missing
		 * subvolume is a deletion in flight, and reconstructing it live
		 * would revert the deletion; the sweep owns those keys.
		 * Anything else falls through to the conservative arms below,
		 * which strip the reference.
		 */
		bool snapshot_agrees = !snapshot_ret &&
			bch2_snapshot_state_compat(&snapshot) == SNAPSHOT_STATE_live &&
			le32_to_cpu(snapshot.subvol) == u.bi_subvol;

		if (ret &&
		    ((c->sb.btrees_lost_data & BIT_ULL(BTREE_ID_subvolumes)) ||
		     snapshot_agrees)) {
			ret = 0;
			try(bch2_reconstruct_subvol(trans, k.k->p.snapshot, u.bi_subvol, u.bi_inum));
			goto do_update;
		}

		printbuf_reset(&buf);
		bch2_inode_unpacked_to_text(&buf, &u);
		prt_printf(&buf, "\nsnapshot %u: ", u.bi_snapshot);
		if (!snapshot_ret)
			bch2_snapshot_to_text(&buf, &snapshot);
		else
			prt_str(&buf, "(missing)");

		/*
		 * No exemption for a missing subvolume: an unlinked subvolume
		 * still resolves, and once it's tombstoned the snapshot is
		 * will_delete and we never visit these keys - the sweep owns
		 * them. A root inode pointing at a subvolume that's actually
		 * gone is damage, and the repair below must run or
		 * check_unreachable_inodes() meets an orphan whose bi_subvol
		 * points nowhere and the reattach fail-stops the pass (field
		 * report, 2026-07-21).
		 *
		 * A reference to the root subvolume is never the broken side:
		 * subvol 1's existence is an invariant, and check_root()
		 * recreates it if lost. Stripping bi_subvol around the
		 * absence takes a valid backref off the root inode - and
		 * check_unreachable_inodes() then reattaches the root
		 * directory into lost+found:
		 */
		bool root_subvol_ref = ret && u.bi_subvol == BCACHEFS_ROOT_SUBVOL;

		if (!root_subvol_ref &&
		    (fsck_err_on(ret,
				trans, inode_bi_subvol_missing,
				"inode bi_subvol points to missing subvolume %u\n%s",
				u.bi_subvol, buf.buf) ||
		     fsck_err_on(le64_to_cpu(s.inode) != u.bi_inum ||
				!bch2_snapshot_is_ancestor(trans, le32_to_cpu(s.snapshot),
							   k.k->p.snapshot),
				trans, inode_bi_subvol_wrong,
				"inode points to subvol %u, but subvol points to %llu:%u\n%s",
				u.bi_subvol,
				le64_to_cpu(s.inode),
				le32_to_cpu(s.snapshot),
				buf.buf))) {
			u.bi_subvol = 0;
			u.bi_parent_subvol = 0;
			do_update = true;
		}
		ret = 0;
	}

	if (fsck_err_on(u.bi_journal_seq > journal_cur_seq(&c->journal),
			trans, inode_journal_seq_in_future,
			"inode journal seq in future (currently at %llu)\n%s",
			journal_cur_seq(&c->journal),
			(printbuf_reset(&buf),
			 bch2_inode_unpacked_to_text(&buf, &u),
			buf.buf))) {
		u.bi_journal_seq = journal_cur_seq(&c->journal);
		do_update = true;
	}
do_update:
	if (do_update) {
		ret = __bch2_fsck_write_inode(trans, &u);
		bch_err_msg(c, ret, "in fsck updating inode");
		if (ret)
			return ret;
	}
fsck_err:
	return ret;
}

int bch2_check_inodes(struct bch_fs *c)
{
	struct bch_inode_unpacked snapshot_root = {};

	CLASS(btree_trans, trans)(c);
	CLASS(snapshots_seen, s)();

	bch2_progress_init(&c->recovery.progress, __func__, c, BIT_ULL(BTREE_ID_inodes), 0);

	return for_each_btree_key_commit(trans, iter, BTREE_ID_inodes,
				POS_MIN,
				BTREE_ITER_prefetch|BTREE_ITER_all_snapshots, k,
				NULL, NULL, BCH_TRANS_COMMIT_no_enospc, ({
		bch2_progress_update_iter(trans, &c->recovery.progress, &iter) ?:
		check_inode(trans, &iter, k, &snapshot_root, &s);
	}));
}

static int find_oldest_inode_needs_reattach(struct btree_trans *trans,
					    struct bch_inode_unpacked *inode)
{
	struct bkey_s_c k;
	int ret = 0;

	/*
	 * We look for inodes to reattach in natural key order, leaves first,
	 * but we should do the reattach at the oldest version that needs to be
	 * reattached:
	 */
	for_each_btree_key_norestart(trans, iter,
				     BTREE_ID_inodes,
				     SPOS(0, inode->bi_inum, inode->bi_snapshot + 1),
				     BTREE_ITER_all_snapshots, k, ret) {
		if (k.k->p.offset != inode->bi_inum)
			break;

		if (!bch2_snapshot_is_ancestor(trans, inode->bi_snapshot, k.k->p.snapshot))
			continue;

		if (!bkey_is_inode(k.k))
			break;

		struct bch_inode_unpacked parent_inode;
		bch2_inode_unpack(trans->c, k, &parent_inode);

		if (!inode_should_reattach(&parent_inode))
			break;

		*inode = parent_inode;
	}

	return ret;
}

/*
 * An unreachable inode version may still be attached in a descendant
 * snapshot: incomplete snapshot deletion can move a dirent further down the
 * snapshot tree than the inode that points to it (an interrupted pass
 * resumes against new topology, so the two stop at different termini),
 * leaving ancestor views orphaned while the descendant view is intact.
 *
 * check_inodes zeroed this version's backpointer, but the attached
 * descendant version still carries its verified one - find that dirent, so
 * we can propagate a copy up to this version's snapshot instead of
 * reattaching in lost+found.
 *
 * Requirements checked here: the dirent names this inode from a strict
 * descendant, the parent directory is visible (and not unlinked) in this
 * version's view, the dirent's name hashes to its offset under that
 * directory's hash info (a mismatched seed - e.g. a reconstructed directory
 * inode - would get the propagated dirent rehashed by the next
 * check_dirents, dangling every backpointer to it), and the destination
 * slot is empty - a whiteout there means the entry was deliberately deleted
 * in this view, and must not be resurrected.
 *
 * Returns a copy in transaction memory: the caller uses it across
 * fsck_err(), which can cycle transaction locks, so a reference into a
 * btree node buffer would be a use after unlock.
 */
static struct bkey_i *find_attached_dirent_in_descendant(struct btree_trans *trans,
					struct bch_inode_unpacked *inode)
{
	struct bch_fs *c = trans->c;
	struct bch_inode_unpacked child;
	bool found = false;
	struct bkey_s_c k;
	int ret = 0;

	/*
	 * Dirents pointing to subvolume roots live in the parent subvolume -
	 * a different snapshot space; those take the reattach path:
	 */
	if (inode->bi_subvol)
		return NULL;

	for_each_btree_key_norestart(trans, iter, BTREE_ID_inodes,
				     SPOS(0, inode->bi_inum, 0),
				     BTREE_ITER_all_snapshots, k, ret) {
		if (k.k->p.offset != inode->bi_inum ||
		    k.k->p.snapshot >= inode->bi_snapshot)
			break;

		if (!bkey_is_inode(k.k) ||
		    !bch2_snapshot_is_ancestor(trans, k.k->p.snapshot, inode->bi_snapshot))
			continue;

		bch2_inode_unpack(c, k, &child);
		if (bch2_inode_has_backpointer(&child) && !child.bi_parent_subvol) {
			found = true;
			break;
		}
	}
	if (ret)
		return ERR_PTR(ret);
	if (!found)
		return NULL;

	u32 snapshot = child.bi_snapshot;
	CLASS(btree_iter_uninit, dirent_iter)(trans);
	struct bkey_s_c_dirent d = bch2_inode_get_dirent(trans, &dirent_iter, &child, &snapshot);
	ret = bkey_err(d);
	if (bch2_err_matches(ret, ENOENT))
		return NULL;
	if (ret)
		return ERR_PTR(ret);

	if (dirent_points_to_inode_nowarn(c, d, inode))
		return NULL;

	/*
	 * Classify our snapshot's view of that position. (d is visible at
	 * the descendant, so it lies on the descendant's rootward path and
	 * is always comparable with our snapshot - no separate ancestry
	 * check is needed.)
	 *
	 * - matching dirent already visible (a propagation done at an older
	 *   version of this inode earlier in the pass): only the
	 *   backpointer needs fixing. No further requirements - there's no
	 *   insert, and falling back to lost+found would create a duplicate
	 *   link to a reachable file
	 * - nothing visible and the slot is empty: propagate a copy, which
	 *   also requires the parent directory to be visible and not
	 *   unlinked. (No hash check needed: check_dirents verified the
	 *   dirent at the descendant's view, and hash info is invariant
	 *   across an inode's snapshot versions - enforced by check_inodes -
	 *   so it hashes identically under the directory here.)
	 * - a different inode's dirent visible: the name belongs to someone
	 *   else in this view, and inserting over it would hide that file
	 *   from every view below; a whiteout in the slot: the entry was
	 *   deliberately deleted here. Both fall back to lost+found.
	 */
	CLASS(btree_iter, vis_iter)(trans, BTREE_ID_dirents,
				    SPOS(d.k->p.inode, d.k->p.offset, inode->bi_snapshot), 0);
	struct bkey_s_c vis = bch2_btree_iter_peek_slot(&vis_iter);
	ret = bkey_err(vis);
	if (ret)
		return ERR_PTR(ret);

	if (vis.k->type == KEY_TYPE_dirent) {
		if (dirent_points_to_inode_nowarn(c, bkey_s_c_to_dirent(vis), inode))
			return NULL;
	} else {
		struct bch_inode_unpacked dir;
		ret = bch2_inode_find_by_inum_snapshot(trans, d.k->p.inode,
						       inode->bi_snapshot, &dir, 0);
		if (bch2_err_matches(ret, ENOENT))
			return NULL;
		if (ret)
			return ERR_PTR(ret);

		if (dir.bi_flags & BCH_INODE_unlinked)
			return NULL;

		CLASS(btree_iter, dst_iter)(trans, BTREE_ID_dirents,
					    SPOS(d.k->p.inode, d.k->p.offset, inode->bi_snapshot),
					    BTREE_ITER_all_snapshots);
		struct bkey_s_c dst = bch2_btree_iter_peek_slot(&dst_iter);
		ret = bkey_err(dst);
		if (ret)
			return ERR_PTR(ret);
		if (!bkey_deleted(dst.k))
			return NULL;
	}

	return bch2_bkey_make_mut_noupdate(trans, d.s_c);
}

static int reattach_via_descendant_dirent(struct btree_trans *trans,
					  struct bch_inode_unpacked *inode,
					  struct bkey_i *new)
{
	struct bch_fs *c = trans->c;

	new->k.p.snapshot = inode->bi_snapshot;

	/*
	 * Re-classify under the intent lock: the probe ran before fsck_err(),
	 * which can cycle transaction locks, and this pass can run online. A
	 * matching dirent that's become visible only needs the backpointer
	 * set; anything else now occupying the position must not be
	 * clobbered.
	 */
	CLASS(btree_iter, vis_iter)(trans, BTREE_ID_dirents, new->k.p,
				    BTREE_ITER_intent);
	struct bkey_s_c vis = bkey_try(bch2_btree_iter_peek_slot(&vis_iter));

	bool have_dirent = vis.k->type == KEY_TYPE_dirent &&
		!dirent_points_to_inode_nowarn(c, bkey_s_c_to_dirent(vis), inode);

	if (!have_dirent) {
		if (vis.k->type == KEY_TYPE_dirent)
			goto bail;

		CLASS(btree_iter, dst_iter)(trans, BTREE_ID_dirents, new->k.p,
					    BTREE_ITER_all_snapshots|BTREE_ITER_intent);
		struct bkey_s_c dst = bkey_try(bch2_btree_iter_peek_slot(&dst_iter));
		if (!bkey_deleted(dst.k))
			goto bail;

		try(bch2_trans_update(trans, &dst_iter, new, BTREE_UPDATE_internal_snapshot_node));
	}

	inode->bi_dir		= new->k.p.inode;
	inode->bi_dir_offset	= new->k.p.offset;
	return __bch2_fsck_write_inode(trans, inode);
bail:
	bch_err(c, "not propagating dirent for inode %llu:%u: destination %llu:%llu:%u now occupied",
		inode->bi_inum, inode->bi_snapshot,
		new->k.p.inode, new->k.p.offset, new->k.p.snapshot);
	return 0;
}

/*
 * Is this inode number a subvolume root? Answered once per inum, from the first
 * version we see.
 *
 * bi_subvol cannot be read off an arbitrary version. Taking a snapshot updates
 * the root inode of the new subvolume but not of the old, so the version left
 * behind at the now-interior node is still the live root of the old subvolume
 * and was never rewritten; versions older still may predate the subvolume
 * entirely. An old version of a subvolume root legitimately reads bi_subvol ==
 * 0, and trusting that is how we ended up reattaching one into lost+found.
 *
 * The first version we see for an inum is different, and one bool taken from it
 * then carries to the rest:
 *
 *  1. We iterate BTREE_ID_inodes with all_snapshots from POS_MIN, and inode
 *     keys sort by (inum, snapshot) - so within an inum we visit snapshot IDs
 *     in ascending order.
 *  2. A snapshot's ID is always strictly less than its parent's; the snapshot
 *     key validator enforces it (snapshot_parent_bad, bch2_snapshot_validate()).
 *     So every descendant of a node sorts before that node.
 *  3. Version B shadows version A only if B lives at a descendant of A's
 *     snapshot. By (2) B sorts before A, so by (1) we would already have seen
 *     B when we reach A.
 *  4. Hence nothing shadows the first version we see for an inum: some live
 *     view resolves to it. That is the version fsck maintains bi_subvol on -
 *     check_subvols() ran before us and repairs it there, and check_inode()
 *     only validates bi_subvol where it is meaningful.
 *  5. Whether an inum is a subvolume root is a property of the number, not of
 *     any one version, so the answer is good for all of them.
 *
 * Only the boolean is carried, not the subvolume ID: the first version we land
 * on may belong to any of the subvolumes rooted at this inum, and which one it
 * is says nothing.
 */
struct subvol_root_seen {
	u64	inum;
	bool	is_subvol_root;
};

static int check_unreachable_inode(struct btree_trans *trans,
				   struct btree_iter *iter,
				   struct bkey_s_c k,
				   struct subvol_root_seen *seen)
{
	CLASS(printbuf, buf)();
	int ret = 0;

	if (!bkey_is_inode(k.k))
		return 0;

	struct bch_inode_unpacked inode;
	bch2_inode_unpack(trans->c, k, &inode);

	/* Before the early return below: every version has to advance this. */
	if (inode.bi_inum != seen->inum) {
		seen->inum		= inode.bi_inum;
		seen->is_subvol_root	= inode.bi_subvol != 0;
	}

	if (!inode_should_reattach(&inode))
		return 0;

	/*
	 * Not for a subvolume root. A subvolume root has exactly one dirent,
	 * in the parent subvolume, and dirents to subvolumes aren't versioned
	 * - so there is no chain of unreachable ancestor versions to walk back
	 * to, and the version we were handed is the one to reattach.
	 *
	 * Note that leaf-ness can't stand in for this: taking a snapshot
	 * updates the root inode of the new subvolume, but not of the old, so
	 * a live subvolume's root inode key stays at a snapshot that has since
	 * become interior.
	 *
	 * Climbing anyway picks some ancestor version, reattaches that - and
	 * because the ancestor doesn't carry bi_subvol, it gets filed into
	 * lost+found as a plain directory named after its inode number, whose
	 * backpointer is then propagated back down over the live versions
	 * below it. The subvolume root ends up reachable both by its own
	 * DT_SUBVOL dirent and by the manufactured one, which is
	 * inode_dir_multiple_links -> emergency read-only at runtime.
	 * (field report, 2026-08-04)
	 */
	if (!seen->is_subvol_root)
		try(find_oldest_inode_needs_reattach(trans, &inode));

	/*
	 * Attached in a descendant snapshot? Then this version has a proper
	 * home; propagate the dirent up to our snapshot rather than
	 * manufacturing a lost+found entry visible in every view below:
	 */
	struct bkey_i *d = errptr_try(find_attached_dirent_in_descendant(trans, &inode));

	if (d) {
		if (inode_fsck_err(trans, SPOS(0, inode.bi_inum, inode.bi_snapshot),
				   inode_unreachable_dirent_in_descendant,
			     "unreachable inode with dirent in descendant snapshot %u, propagating:\n%s",
			     d->k.p.snapshot,
			     (bch2_inode_unpacked_to_text(&buf, &inode),
			      buf.buf)))
			try(reattach_via_descendant_dirent(trans, &inode, d));
		return ret;
	}

	if (inode_fsck_err(trans, SPOS(0, inode.bi_inum, inode.bi_snapshot),
			   inode_unreachable,
		     "unreachable inode:\n%s",
		     (bch2_inode_unpacked_to_text(&buf, &inode),
		      buf.buf)))
		try(bch2_reattach_inode(trans, &inode));
fsck_err:
	return ret;
}

/*
 * Reattach unreachable (but not unlinked) inodes
 *
 * Run after check_inodes() and check_dirents(), so we node that inode
 * backpointer fields point to valid dirents, and every inode that has a dirent
 * that points to it has its backpointer field set - so we're just looking for
 * non-unlinked inodes without backpointers:
 *
 * XXX: this is racy w.r.t. hardlink removal in online fsck
 */
int bch2_check_unreachable_inodes(struct bch_fs *c)
{
	bch2_progress_init(&c->recovery.progress, __func__, c, BIT_ULL(BTREE_ID_inodes), 0);

	struct subvol_root_seen seen = {};

	CLASS(btree_trans, trans)(c);
	return for_each_btree_key_commit(trans, iter, BTREE_ID_inodes,
				POS_MIN,
				BTREE_ITER_prefetch|BTREE_ITER_all_snapshots, k,
				NULL, NULL, BCH_TRANS_COMMIT_no_enospc, ({
		bch2_progress_update_iter(trans, &c->recovery.progress, &iter) ?:
		check_unreachable_inode(trans, &iter, k, &seen);
	}));
}

static inline bool btree_matches_i_mode(enum btree_id btree, unsigned mode)
{
	switch (btree) {
	case BTREE_ID_extents:
		return S_ISREG(mode) || S_ISLNK(mode);
	case BTREE_ID_dirents:
		return S_ISDIR(mode);
	case BTREE_ID_xattrs:
		return true;
	default:
		BUG();
	}
}

static int count_inode_keys(struct btree_trans *trans,
			    struct bpos inode_pos,
			    enum btree_id btree,
			    struct printbuf *out)
{
	struct bkey_s_c k;
	unsigned nr_keys = 0;
	int ret = 0;
	for_each_btree_key_max_norestart(trans, iter, btree,
					 inode_pos,
					 POS(inode_pos.inode, U64_MAX),
					 0, k, ret) {
		/*
		 * Error keys count: they're placeholders for unreadable data,
		 * evidence the inode had contents. Hash whiteouts are just
		 * tombstones:
		 */
		if (k.k->type == KEY_TYPE_hash_whiteout)
			continue;

		nr_keys++;
		if (out && nr_keys <= 10) {
			bch2_bkey_val_to_text(out, trans->c, k);
			prt_newline(out);
		}
		if (nr_keys >= 100)
			break;
	}

	return ret ?: nr_keys;
}

int bch2_check_key_has_inode(struct btree_trans *trans,
			     struct btree_iter *iter,
			     struct inode_walker *inode,
			     struct inode_walker_entry *i,
			     struct bkey_s_c k)
{
	errptr_try(i);

	/* whiteouts and hash whiteouts are tombstones - they need no inode: */
	if (bkey_extent_whiteout(k.k) ||
	    k.k->type == KEY_TYPE_hash_whiteout)
		return 0;

	bool have_inode = i && !i->whiteout;

	if (have_inode && btree_matches_i_mode(iter->btree_id, i->inode.bi_mode))
		return 0;

	struct bch_fs *c = trans->c;
	CLASS(printbuf, buf)();

	if (have_inode)
		prt_printf(&buf, "key for wrong inode mode %o", i->inode.bi_mode);
	else
		prt_str(&buf, "key in missing inode");

	struct inode_walker_entry *good_ancestor = NULL;
	darray_for_each(inode->inodes, i2)
		if (!i2->whiteout &&
		    bch2_snapshot_is_ancestor(trans, k.k->p.snapshot, i2->inode.bi_snapshot) &&
		    btree_matches_i_mode(iter->btree_id, i2->inode.bi_mode)) {
			prt_printf(&buf, ", but found good inode in older snapshot");
			bch2_inode_unpacked_to_text(&buf, &i2->inode);
			prt_newline(&buf);
			good_ancestor = i2;
			break;
		}

	prt_printf(&buf, "\nfound keys:\n");

	struct bpos inode_pos = SPOS(k.k->p.inode, 0, k.k->p.snapshot);
	int ret = count_inode_keys(trans, inode_pos, iter->btree_id, &buf);
	if (ret < 0)
		return ret;

	unsigned nr_keys = ret;
	if (!nr_keys) {
		bch_err(c, "%s: error finding live keys in inode", __func__);
		return bch_err_throw(c, shutdown_with_errors_unfixed);
	}

	if (nr_keys > 100)
		prt_printf(&buf, "found > %u keys for this inode\n", nr_keys);
	else
		prt_printf(&buf, "found %u keys for this inode\n", nr_keys);

	if (c->sb.btrees_lost_data & BIT_ULL(BTREE_ID_inodes))
		prt_str(&buf, "data was lost in inodes btree\n");

	if (!have_inode) {
		bool inode_looks_deleted =
			good_ancestor &&
			nr_keys < 3 &&
			!(c->sb.btrees_lost_data & BIT_ULL(BTREE_ID_inodes));
		if (inode_looks_deleted)
			prt_str(&buf, "inode was deleted, will delete key\n");

		if (ret_inode_fsck_err(trans, k.k->p,
				       key_in_missing_inode, "%s", buf.buf)) {
			if (inode_looks_deleted)
				return bch2_btree_delete_at(trans, iter, BTREE_UPDATE_internal_snapshot_node);

			if (!good_ancestor) {
				try(reconstruct_inode(trans, iter->btree_id, k.k->p.snapshot, k.k->p.inode));
				try(bch2_trans_commit(trans, NULL, NULL, BCH_TRANS_COMMIT_no_enospc));

				inode->last_pos.inode--;
				return btree_trans_restart(trans, BCH_ERR_transaction_restart_commit);
			} else {
				u32 snapshot = i->inode.bi_snapshot;
				i->inode = good_ancestor->inode;
				i->inode.bi_snapshot = snapshot;
				/*
				 * __ (non-committing) version: we're inside the
				 * caller's commit_do(). The self-committing one
				 * eats restarts, and when the following lazy
				 * commit then has nothing to do it returns 0 -
				 * leaking the advanced restart_count to the
				 * caller's verify (panic, restart_count N
				 * should be M):
				 */
				try(__bch2_fsck_write_inode(trans, &i->inode));
				try(bch2_trans_commit_lazy(trans, NULL, NULL, BCH_TRANS_COMMIT_no_enospc));
			}
		}
	} else {
		if (ret_inode_fsck_err(trans, k.k->p,
				       key_in_wrong_inode_type, "%s", buf.buf)) {
			int nr_extents = iter->btree_id == BTREE_ID_extents
				? nr_keys : count_inode_keys(trans, inode_pos, BTREE_ID_extents, NULL);
			if (nr_extents < 0)
				return nr_extents;

			int nr_dirents = iter->btree_id == BTREE_ID_dirents
				? nr_keys : count_inode_keys(trans, inode_pos, BTREE_ID_dirents, NULL);
			if (nr_dirents < 0)
				return nr_dirents;

			if (nr_extents && nr_dirents) {
				bch_err(c, "have both extents and dirents for inode with bad mode, cannot repair");
				return bch_err_throw(c, shutdown_with_errors_unfixed);
			}

			i->inode.bi_mode &= ~S_IFMT;

			if (nr_dirents)
				i->inode.bi_mode |= S_IFDIR;
			else
				i->inode.bi_mode |= S_IFREG;

			/* __: see above - don't eat restarts under the caller's commit_do() */
			try(__bch2_fsck_write_inode(trans, &i->inode));
			try(bch2_trans_commit_lazy(trans, NULL, NULL, BCH_TRANS_COMMIT_no_enospc));
		}
	}

	return 0;
}

static int maybe_reconstruct_inum_btree(struct btree_trans *trans,
					u64 inum, u32 snapshot,
					enum btree_id btree)
{
	struct bkey_s_c k;
	int ret = 0;

	for_each_btree_key_max_norestart(trans, iter, btree,
					 SPOS(inum, 0, snapshot),
					 POS(inum, U64_MAX),
					 0, k, ret) {
		ret = 1;
		break;
	}

	if (ret <= 0)
		return ret;

	if (inode_fsck_err(trans, SPOS(0, inum, snapshot),
		     missing_inode_with_contents,
		     "inode %llu:%u type %s missing, but contents found: reconstruct?",
		     inum, snapshot,
		     btree == BTREE_ID_extents ? "reg" : "dir"))
		return  reconstruct_inode(trans, btree, snapshot, inum) ?:
			bch2_trans_commit(trans, NULL, NULL, BCH_TRANS_COMMIT_no_enospc) ?:
			btree_trans_restart(trans, BCH_ERR_transaction_restart_commit);
fsck_err:
	return ret;
}

static int maybe_reconstruct_inum(struct btree_trans *trans,
				  u64 inum, u32 snapshot)
{
	return  maybe_reconstruct_inum_btree(trans, inum, snapshot, BTREE_ID_extents) ?:
		maybe_reconstruct_inum_btree(trans, inum, snapshot, BTREE_ID_dirents);
}

static int check_subdir_count_notnested(struct btree_trans *trans, struct inode_walker *w)
{
	struct bch_fs *c = trans->c;
	int ret = 0;
	s64 count2;

	darray_for_each(w->inodes, i) {
		if (i->inode.bi_nlink == i->count)
			continue;

		count2 = bch2_count_subdirs(trans, w->last_pos.inode, i->inode.bi_snapshot);
		if (count2 < 0)
			return count2;

		if (w->recalculate_sums)
			i->count = count2;

		if (i->count != count2) {
			bch_err_ratelimited(c, "fsck counted subdirectories wrong for inum %llu:%u: got %llu should be %llu",
					    w->last_pos.inode, i->inode.bi_snapshot, i->count, count2);
			i->count = count2;
			if (i->inode.bi_nlink == i->count)
				continue;
		}

		if (i->inode.bi_nlink != i->count) {
			CLASS(printbuf, buf)();

			lockrestart_do(trans,
				       bch2_inum_snapshot_to_path(trans, w->last_pos.inode,
								  i->inode.bi_snapshot, NULL, &buf));
			prt_newline(&buf);
			bch2_inode_unpacked_to_text(&buf, &i->inode);

			if (fsck_err_on(i->inode.bi_nlink != i->count,
					trans, inode_dir_wrong_nlink,
					"directory with wrong i_nlink: got %u, should be %llu\n%s",
					i->inode.bi_nlink, i->count, buf.buf)) {
				i->inode.bi_nlink = i->count;
				ret = bch2_fsck_write_inode(trans, &i->inode);
				if (ret)
					break;
			}
		}
	}
fsck_err:
	bch_err_fn(c, ret);
	return ret;
}

static int check_subdir_dirents_count(struct btree_trans *trans, struct inode_walker *w)
{
	/*
	 * Nested transaction, like check_i_sectors(): the inner
	 * bch2_fsck_write_inode() commits discard the fsck_err logs queued in
	 * check_subdir_count_notnested() and we return a restart - exempt those
	 * begins from the dropped-updates warning.
	 */
	u32 restart_count = trans->restart_count;
	trans->begin_may_drop_updates = true;
	int ret = check_subdir_count_notnested(trans, w);
	trans->begin_may_drop_updates = false;

	return ret ?: trans_was_restarted(trans, restart_count);
}

/* find a subvolume that's a descendent of @snapshot: */
static int find_snapshot_subvol(struct btree_trans *trans, u32 snapshot, u32 *subvolid)
{
	struct bkey_s_c k;
	int ret;

	for_each_btree_key_norestart(trans, iter, BTREE_ID_subvolumes, POS_MIN, 0, k, ret) {
		if (k.k->type != KEY_TYPE_subvolume)
			continue;

		struct bkey_s_c_subvolume s = bkey_s_c_to_subvolume(k);
		if (bch2_snapshot_is_ancestor(trans, le32_to_cpu(s.v->snapshot), snapshot)) {
			*subvolid = k.k->p.offset;
			return 0;
		}
	}

	return ret ?: -ENOENT;
}

noinline_for_stack
static int check_dirent_to_subvol(struct btree_trans *trans, struct btree_iter *iter,
				  struct bkey_s_c_dirent d)
{
	struct bch_fs *c = trans->c;
	CLASS(btree_iter_uninit, subvol_iter)(trans);
	struct bch_inode_unpacked subvol_root;
	u32 parent_subvol = le32_to_cpu(d.v->d_parent_subvol);
	u32 target_subvol = le32_to_cpu(d.v->d_child_subvol);
	u32 parent_snapshot;
	u32 new_parent_subvol = 0;
	CLASS(printbuf, buf)();
	int ret = 0;

	ret = bch2_subvolume_get_snapshot(trans, parent_subvol, &parent_snapshot);
	if (ret && !bch2_err_matches(ret, ENOENT))
		return ret;

	if (ret && parent_subvol == BCACHEFS_ROOT_SUBVOL) {
		/*
		 * A reference to the root subvolume is never the broken side:
		 * subvol 1's existence is an invariant, and check_root()
		 * recreates it if lost. Rewiring parent_subvol around the
		 * absence reparents the root directory's subvolume dirents
		 * onto arbitrary subvolumes; left alone, they're valid the
		 * moment the root subvolume is back:
		 */
		ret = 0;
		goto check_target;
	}

	if (ret ||
	    (!ret && !bch2_snapshot_is_ancestor(trans, parent_snapshot, d.k->p.snapshot))) {
		ret = find_snapshot_subvol(trans, d.k->p.snapshot, &new_parent_subvol);
		if (ret && !bch2_err_matches(ret, ENOENT))
			return ret;
	}

	if (ret &&
	    !new_parent_subvol &&
	    (c->sb.btrees_lost_data & BIT_ULL(BTREE_ID_subvolumes))) {
		ret = 0;
		/*
		 * Couldn't find a subvol for dirent's snapshot - but we lost
		 * subvols, so we need to reconstruct:
		 */
		try(bch2_reconstruct_subvol(trans, d.k->p.snapshot, parent_subvol, 0));

		parent_snapshot = d.k->p.snapshot;
	}

	if (fsck_err_on(ret,
			trans, dirent_to_missing_parent_subvol,
			"dirent parent_subvol points to missing subvolume\n%s",
			(bch2_bkey_val_to_text(&buf, c, d.s_c), buf.buf)) ||
	    fsck_err_on(!ret && !bch2_snapshot_is_ancestor(trans, parent_snapshot, d.k->p.snapshot),
			trans, dirent_not_visible_in_parent_subvol,
			"dirent not visible in parent_subvol (not an ancestor of subvol snap %u)\n%s",
			parent_snapshot,
			(bch2_bkey_val_to_text(&buf, c, d.s_c), buf.buf))) {
		if (!new_parent_subvol) {
			bch_err(c, "could not find a subvol for snapshot %u", d.k->p.snapshot);
			return bch_err_throw(c, fsck_repair_unimplemented);
		}

		/*
		 * The dirent is rewritten at its own position, which may be an
		 * interior snapshot node - dirents in old snapshots need this
		 * repair too:
		 */
		struct bkey_i_dirent *new_dirent =
			errptr_try(bch2_bkey_make_mut_typed(trans, iter, &d.s_c,
						BTREE_UPDATE_internal_snapshot_node, dirent));

		new_dirent->v.d_parent_subvol = cpu_to_le32(new_parent_subvol);

		/*
		 * The fs_path_parent check below repairs the subvolume to agree
		 * with the dirent, so it has to agree with the dirent we just
		 * wrote and not the one we replaced - otherwise a single pass
		 * writes two different answers, and each subsequent fsck moves
		 * one to match the other's stale value.
		 */
		parent_subvol = new_parent_subvol;
	}

check_target:
	bch2_trans_iter_init(trans, &subvol_iter, BTREE_ID_subvolumes, POS(0, target_subvol), 0);
	struct bkey_s_c_subvolume s = bch2_bkey_get_typed(&subvol_iter, subvolume);
	ret = bkey_err(s.s_c);
	if (ret && !bch2_err_matches(ret, ENOENT))
		return ret;

	/*
	 * A deleted-state subvolume is a tombstone pending the snapshot
	 * sweep: bch2_subvolume_get() reports those as ENOENT, but this is a
	 * raw read (the fs_path_parent repair below needs the key). Treat it
	 * as missing here too - otherwise the dirent outlives the subvolume,
	 * and fsck stops converging once the sweep removes the key:
	 */
	if (!ret && bch2_subvolume_state_compat(s.v) == SUBVOLUME_STATE_deleted)
		ret = bch_err_throw(c, ENOENT_subvolume_deleted);

	if (ret) {
		if (inode_fsck_err(trans, d.k->p,
				   dirent_to_missing_subvol,
			     "dirent points to missing subvolume\n%s",
			     (bch2_bkey_val_to_text(&buf, c, d.s_c), buf.buf)))
			return bch2_fsck_remove_dirent(trans, d.k->p);
		return 0;
	}

	if (le32_to_cpu(s.v->fs_path_parent) != parent_subvol) {
		printbuf_reset(&buf);

		prt_printf(&buf, "subvol with wrong fs_path_parent, should be %u\n",
			   parent_subvol);

		try(bch2_inum_to_path(trans, (subvol_inum) { s.k->p.offset,
				      le64_to_cpu(s.v->inode) }, &buf));
		prt_newline(&buf);
		bch2_bkey_val_to_text(&buf, c, s.s_c);

		if (fsck_err(trans, subvol_fs_path_parent_wrong, "%s", buf.buf)) {
			struct bkey_i_subvolume *n =
				errptr_try(bch2_bkey_make_mut_typed(trans, &subvol_iter, &s.s_c, 0, subvolume));

			n->v.fs_path_parent = cpu_to_le32(parent_subvol);
		}
	}

	u64 target_inum = le64_to_cpu(s.v->inode);
	u32 target_snapshot = le32_to_cpu(s.v->snapshot);

	ret = bch2_inode_find_by_inum_snapshot(trans, target_inum, target_snapshot,
					       &subvol_root, 0);
	if (ret && !bch2_err_matches(ret, ENOENT))
		return ret;

	if (ret) {
		bch_err(c, "subvol %u points to missing inode root %llu", target_subvol, target_inum);
		return bch_err_throw(c, fsck_repair_unimplemented);
	}

	if (fsck_err_on(!ret && parent_subvol != subvol_root.bi_parent_subvol,
			trans, inode_bi_parent_wrong,
			"subvol root %llu has wrong bi_parent_subvol: got %u, should be %u",
			target_inum,
			subvol_root.bi_parent_subvol, parent_subvol)) {
		subvol_root.bi_parent_subvol = parent_subvol;
		subvol_root.bi_snapshot = le32_to_cpu(s.v->snapshot);
		try(__bch2_fsck_write_inode(trans, &subvol_root));
	}

	try(bch2_check_dirent_target(trans, iter, d, &subvol_root, true));
fsck_err:
	return ret;
}

static int check_dirent(struct btree_trans *trans, struct btree_iter *iter,
			struct bkey_s_c k,
			struct bch_hash_info *hash_info,
			struct inode_walker *dir,
			struct inode_walker *target,
			struct snapshots_seen *s,
			bool *need_second_pass)
{
	struct bch_fs *c = trans->c;
	CLASS(printbuf, buf)();
	int ret = 0;

	ret = bch2_check_key_has_snapshot(trans, iter, k);
	if (ret)
		return ret < 0 ? ret : 0;

	ret = bch2_snapshots_seen_update(c, s, iter->btree_id, k.k->p);
	if (ret)
		return ret;

	if (k.k->type == KEY_TYPE_whiteout)
		return 0;

	if (dir->last_pos.inode != k.k->p.inode && dir->have_inodes)
		try(check_subdir_dirents_count(trans, dir));

	struct inode_walker_entry *i = errptr_try(bch2_walk_inode(trans, dir, k));

	try(bch2_check_key_has_inode(trans, iter, dir, i, k));

	if (!i || i->whiteout)
		return 0;

	if (dir->first_this_inode)
		try(bch2_hash_info_init(c, &i->inode, hash_info));
	dir->first_this_inode = false;

	hash_info->cf_encoding = bch2_inode_casefold(c, &i->inode) ? c->cf_encoding : NULL;

	bool invalidated_inodes = false;
	ret = bch2_str_hash_check_key(trans, s, &bch2_dirent_hash_desc, hash_info,
				      k, need_second_pass, &invalidated_inodes);
	if (invalidated_inodes) {
		dir->last_pos.inode = 0;
		dir->inodes.nr = 0;
		return btree_trans_restart(trans, BCH_ERR_transaction_restart_nested);
	}

	if (ret < 0)
		return ret;
	if (ret)
		return 0; /* dirent has been deleted */
	if (k.k->type != KEY_TYPE_dirent)
		return 0;

	struct bkey_s_c_dirent d = bkey_s_c_to_dirent(k);

	if (d.v->d_type == DT_SUBVOL) {
		try(check_dirent_to_subvol(trans, iter, d));
	} else {
		try(get_visible_inodes(trans, target, s, le64_to_cpu(d.v->d_inum)));

		if (!target->inodes.nr)
			try(maybe_reconstruct_inum(trans, le64_to_cpu(d.v->d_inum), d.k->p.snapshot));

		/*
		 * The inode must exist in an ancestor snapshot of the dirent:
		 * that's what makes the dirent resolvable from every
		 * subvolume leaf that can see it. get_visible_inodes() also
		 * accepts inodes in descendant snapshots - reachable from
		 * *some* leaf, but a subvolume in a sibling branch sees the
		 * dirent and not the inode, and lookups there return ENOENT.
		 *
		 * It iterates from the dirent's snapshot downward, so an
		 * ancestor inode, if there is one, is first in the list:
		 */
		bool have_ancestor = target->inodes.nr &&
			bch2_snapshot_is_ancestor(trans, d.k->p.snapshot,
						  target->inodes.data[0].inode.bi_snapshot);

		if (target->inodes.nr && !have_ancestor) {
			/*
			 * Deleting the dirent would orphan those inodes:
			 * instead, move it to the snapshot(s) where the inode
			 * exists. The second pass revisits the copies for the
			 * target checks and directory counts:
			 */
			if (inode_fsck_err(trans, d.k->p,
				     dirent_to_inode_in_descendant_snapshot,
				     "dirent with inode(s) only in descendant snapshots:\n%s",
				     (printbuf_reset(&buf),
				      bch2_bkey_val_to_text(&buf, c, k),
				      buf.buf))) {
				darray_for_each(target->inodes, i) {
					try(bch2_trans_commit_lazy_if_full(trans, NULL, NULL,
							BCH_TRANS_COMMIT_no_enospc));

					/*
					 * The inode-side state firing this repair is
					 * unchanged by the copies, so a re-drive after a
					 * partially-committed batch fires it again: skip
					 * the copies already committed, or the
					 * commit_lazy_if_full() restart can't make
					 * forward progress:
					 */
					CLASS(btree_iter, probe)(trans, BTREE_ID_dirents,
							SPOS(k.k->p.inode, k.k->p.offset,
							     i->inode.bi_snapshot), 0);
					struct bkey_s_c old =
						bkey_try(bch2_btree_iter_peek_slot(&probe));

					if (old.k->p.snapshot == i->inode.bi_snapshot &&
					    old.k->type == k.k->type &&
					    bkey_val_bytes(old.k) == bkey_val_bytes(k.k) &&
					    !memcmp(old.v, k.v, bkey_val_bytes(k.k)))
						continue;

					struct bkey_i *n =
						errptr_try(bch2_bkey_make_mut_noupdate(trans, k));

					n->k.p.snapshot = i->inode.bi_snapshot;
					try(bch2_btree_insert_trans(trans, BTREE_ID_dirents, n,
							BTREE_UPDATE_internal_snapshot_node));
				}

				/*
				 * Delete with the hash info we already have -
				 * bch2_fsck_remove_dirent() rederives it via
				 * lookup_first_inode(), which takes whatever
				 * snapshot's inode it finds first:
				 */
				CLASS(btree_iter, del_iter)(trans, BTREE_ID_dirents,
							    d.k->p, BTREE_ITER_intent);
				try(bch2_btree_iter_traverse(&del_iter));
				try(bch2_hash_delete_at(trans, bch2_dirent_hash_desc,
							hash_info, &del_iter,
							BTREE_UPDATE_internal_snapshot_node));
				*need_second_pass = true;
				return 0;
			}
		}

		if (inode_fsck_err_on(!target->inodes.nr,
				trans, d.k->p,
				dirent_to_missing_inode,
				"dirent points to missing inode:\n%s",
				(printbuf_reset(&buf),
				 bch2_bkey_val_to_text(&buf, c, k),
				 buf.buf)))
			try(bch2_fsck_remove_dirent(trans, d.k->p));

		darray_for_each(target->inodes, i) {
			/*
			 * One repair per snapshot version of the target inode:
			 * bounded only by snapshot count, so commit-and-restart
			 * before the batch outgrows the trans bump allocator.
			 * The re-drive converges: get_visible_inodes() rereads
			 * the versions and committed repairs no longer fire.
			 */
			try(bch2_trans_commit_lazy_if_full(trans, NULL, NULL,
						BCH_TRANS_COMMIT_no_enospc));

			try(bch2_check_dirent_target(trans, iter, d, &i->inode, true));
		}

		darray_for_each(target->deletes, i) {
			if (snapshot_list_has_id(&s->ids, *i))
				continue;

			try(bch2_trans_commit_lazy_if_full(trans, NULL, NULL,
						BCH_TRANS_COMMIT_no_enospc));

			CLASS(btree_iter, delete_iter)(trans,
					     BTREE_ID_dirents,
					     SPOS(k.k->p.inode, k.k->p.offset, *i),
					     BTREE_ITER_intent);
			/*
			 * The deletes list is derived from inode-side state the
			 * whiteouts don't change: check whether the dirent is
			 * still visible in this snapshot, both so a re-drive
			 * after a partially-committed batch skips the committed
			 * whiteouts (or the commit_lazy_if_full() restart can't
			 * make forward progress), and so an already-invisible
			 * dirent isn't reported and "repaired" redundantly:
			 */
			struct bkey_s_c visible =
				bkey_try(bch2_btree_iter_peek_slot(&delete_iter));
			if (visible.k->type != KEY_TYPE_dirent)
				continue;

			if (inode_fsck_err(trans, visible.k->p,
				     dirent_to_overwritten_inode,
				     "dirent points to inode overwritten in snapshot %u:\n%s",
				     *i,
				     (printbuf_reset(&buf),
				      bch2_bkey_val_to_text(&buf, c, k),
				      buf.buf)))
				try(bch2_hash_delete_at(trans, bch2_dirent_hash_desc,
							hash_info,
							&delete_iter,
							BTREE_UPDATE_internal_snapshot_node));
		}
	}

	/*
	 * Cannot access key values after doing a transaction commit without
	 * revalidating:
	 */
	bool have_dir = d.v->d_type == DT_DIR;

	try(bch2_trans_commit(trans, NULL, NULL, BCH_TRANS_COMMIT_no_enospc));

	if (have_dir)
		for_each_visible_inode(trans, s, dir, d.k->p.snapshot, i)
			i->count++;
fsck_err:
	return ret;
}

/*
 * Walk dirents: verify that they all have a corresponding S_ISDIR inode,
 * validate d_type
 */
int bch2_check_dirents(struct bch_fs *c)
{
	struct bch_hash_info hash_info;
	CLASS(btree_trans, trans)(c);
	CLASS(snapshots_seen, s)();
	CLASS(inode_walker, dir)();
	CLASS(inode_walker, target)();
	bool need_second_pass = false, did_second_pass = false;
	int ret;
again:
	bch2_progress_init(&c->recovery.progress, __func__, c, BIT_ULL(BTREE_ID_dirents), 0);

	ret = for_each_btree_key_commit(trans, iter, BTREE_ID_dirents,
				POS(BCACHEFS_ROOT_INO, 0),
				BTREE_ITER_prefetch|BTREE_ITER_all_snapshots, k,
				NULL, NULL, BCH_TRANS_COMMIT_no_enospc, ({
			bch2_progress_update_iter(trans, &c->recovery.progress, &iter) ?:
			check_dirent(trans, &iter, k, &hash_info, &dir, &target, &s,
				     &need_second_pass);
		}));
	if (!ret) {
		/*
		 * Final flush of the last directory's subdir count. Exempt the
		 * inner fsck_write_inode commits from the dropped-updates warning
		 * as check_subdir_dirents_count() does - but call _notnested
		 * directly, NOT the nested wrapper: that returns
		 * trans_was_restarted() for an in-loop caller to retry on, and at
		 * this post-loop flush the restart has no handler and faults
		 * recovery (it broke every transaction-restart-injection test).
		 */
		trans->begin_may_drop_updates = true;
		ret = check_subdir_count_notnested(trans, &dir);
		trans->begin_may_drop_updates = false;
	}

	if (!ret && need_second_pass && !did_second_pass) {
		bch_info(c, "check_dirents requires second pass");
		swap(did_second_pass, need_second_pass);
		goto again;
	}

	if (!ret && need_second_pass) {
		bch_err(c, "dirents not repairing");
		ret = -EINVAL;
	}

	return ret;
}

static int check_xattr(struct btree_trans *trans, struct btree_iter *iter,
		       struct bkey_s_c k,
		       struct bch_hash_info *hash_info,
		       struct snapshots_seen *s,
		       struct inode_walker *inode)
{
	struct bch_fs *c = trans->c;
	CLASS(printbuf, buf)();

	int ret = bch2_check_key_has_snapshot(trans, iter, k);
	if (ret < 0)
		return ret;
	if (ret)
		return 0;

	try(bch2_snapshots_seen_update(c, s, iter->btree_id, k.k->p));

	struct inode_walker_entry *i = errptr_try(bch2_walk_inode(trans, inode, k));

	try(bch2_check_key_has_inode(trans, iter, inode, i, k));

	if (!i || i->whiteout)
		return 0;

	if (inode->first_this_inode)
		try(bch2_hash_info_init(c, &i->inode, hash_info));
	inode->first_this_inode = false;

	/*
	 * The dangerous direction for the BCH_INODE_has_*_acl flags - flag
	 * clear but an acl xattr exists, which would break the
	 * bch2_get_acl() short circuit - is verified here, where we're
	 * already walking the xattrs; the reverse direction is checked in
	 * check_inode().
	 *
	 * The xattr is visible from every descendant snapshot version of
	 * the inode that hasn't overwritten it - they all need the flag.
	 *
	 * Repairs are on attempt-local copies, without mutating the
	 * walker's - a restarted commit must see the gating check fire
	 * again, and when the repair commits the walker revalidates:
	 */
	if (k.k->type == KEY_TYPE_xattr) {
		unsigned x_type = bkey_s_c_to_xattr(k).v->x_type;
		struct inode_walker_entry *i2;

		/*
		 * One repair per visible snapshot version, bounded only by
		 * snapshot count: commit-and-restart before the batch
		 * outgrows the trans bump allocator. The re-drive converges -
		 * the walker revalidates on commit (commit_count), so
		 * committed flag repairs no longer fire:
		 */
		if (x_type == KEY_TYPE_XATTR_INDEX_POSIX_ACL_ACCESS)
			for_each_visible_inode(trans, s, inode, k.k->p.snapshot, i2) {
				try(bch2_trans_commit_lazy_if_full(trans, NULL, NULL,
							BCH_TRANS_COMMIT_no_enospc));

				if (!i2->whiteout &&
				    fsck_err_on(!(i2->inode.bi_flags & BCH_INODE_has_access_acl),
						trans, inode_has_access_acl_flag_wrong,
						"inode has an access acl xattr but BCH_INODE_has_access_acl not set\n%s",
						(printbuf_reset(&buf),
						 bch2_inode_unpacked_to_text(&buf, &i2->inode),
						 prt_newline(&buf),
						 bch2_bkey_val_to_text(&buf, c, k),
						 buf.buf))) {
					struct bch_inode_unpacked inode_u = i2->inode;
					inode_u.bi_flags |= BCH_INODE_has_access_acl;
					try(__bch2_fsck_write_inode(trans, &inode_u));
				}
			}

		if (x_type == KEY_TYPE_XATTR_INDEX_POSIX_ACL_DEFAULT)
			for_each_visible_inode(trans, s, inode, k.k->p.snapshot, i2) {
				try(bch2_trans_commit_lazy_if_full(trans, NULL, NULL,
							BCH_TRANS_COMMIT_no_enospc));

				if (!i2->whiteout &&
				    fsck_err_on(!(i2->inode.bi_flags & BCH_INODE_has_default_acl),
						trans, inode_has_default_acl_flag_wrong,
						"inode has a default acl xattr but BCH_INODE_has_default_acl not set\n%s",
						(printbuf_reset(&buf),
						 bch2_inode_unpacked_to_text(&buf, &i2->inode),
						 prt_newline(&buf),
						 bch2_bkey_val_to_text(&buf, c, k),
						 buf.buf))) {
					struct bch_inode_unpacked inode_u = i2->inode;
					inode_u.bi_flags |= BCH_INODE_has_default_acl;
					try(__bch2_fsck_write_inode(trans, &inode_u));
				}
			}
	}

	bool need_second_pass = false;
	bool invalidated_inodes = false;
	ret = bch2_str_hash_check_key(trans, NULL, &bch2_xattr_hash_desc, hash_info,
				      k, &need_second_pass, &invalidated_inodes);
	if (invalidated_inodes) {
		inode->last_pos.inode--;
		return btree_trans_restart(trans, BCH_ERR_transaction_restart_nested);
	}

	ret = min(ret, 0);
fsck_err:
	return ret;
}

/*
 * Walk xattrs: verify that they all have a corresponding inode
 */
int bch2_check_xattrs(struct bch_fs *c)
{
	struct bch_hash_info hash_info;
	CLASS(btree_trans, trans)(c);
	CLASS(snapshots_seen, s)();
	CLASS(inode_walker, inode)();

	bch2_progress_init(&c->recovery.progress, __func__, c, BIT_ULL(BTREE_ID_xattrs), 0);

	int ret = for_each_btree_key_commit(trans, iter, BTREE_ID_xattrs,
			POS(BCACHEFS_ROOT_INO, 0),
			BTREE_ITER_prefetch|BTREE_ITER_all_snapshots,
			k,
			NULL, NULL,
			BCH_TRANS_COMMIT_no_enospc, ({
		bch2_progress_update_iter(trans, &c->recovery.progress, &iter) ?:
		check_xattr(trans, &iter, k, &hash_info, &s, &inode);
	}));
	return ret;
}

static int check_root_trans(struct btree_trans *trans)
{
	struct bch_fs *c = trans->c;

	u32 snapshot;
	u64 inum;
	int ret = subvol_lookup(trans, BCACHEFS_ROOT_SUBVOL, &snapshot, &inum);
	if (ret && !bch2_err_matches(ret, ENOENT))
		return ret;

	/*
	 * The missing subvolume key is not the only record of root's active
	 * snapshot: snapshot nodes carry a subvol backref, so root's active
	 * snapshot is the leaf claiming BCACHEFS_ROOT_SUBVOL. Only when no
	 * node does (fresh filesystem, or the snapshots btree is gone too)
	 * is U32_MAX - the initial snapshot id - correct; on a snapshotted
	 * filesystem U32_MAX is an interior node, and a subvolume pointing
	 * at it is subvol_snapshot_not_leaf, which has no repair.
	 *
	 * _norestart: we're inside the caller's commit_do(), so restarts must
	 * propagate out to it:
	 */
	u32 root_snapshot = 0;
	if (ret) {
		struct bkey_s_c k;
		int ret2 = 0;

		for_each_btree_key_norestart(trans, iter, BTREE_ID_snapshots,
					     POS_MIN, 0, k, ret2) {
			if (k.k->type == KEY_TYPE_snapshot) {
				struct bkey_s_c_snapshot s = bkey_s_c_to_snapshot(k);

				if (le32_to_cpu(s.v->subvol) == BCACHEFS_ROOT_SUBVOL &&
				    !s.v->children[0]) {
					root_snapshot = k.k->p.offset;
					break;
				}
			}
		}
		try(ret2);
	}

	if (mustfix_fsck_err_on(ret, trans, root_subvol_missing,
				"root subvol missing")) {
		struct bkey_i_subvolume *root_subvol =
			errptr_try(bch2_trans_kmalloc(trans, sizeof(*root_subvol)));

		snapshot	= root_snapshot ?: U32_MAX;
		inum		= BCACHEFS_ROOT_INO;

		bkey_subvolume_init(&root_subvol->k_i);
		root_subvol->k.p.offset = BCACHEFS_ROOT_SUBVOL;
		root_subvol->v.flags	= 0;
		root_subvol->v.snapshot	= cpu_to_le32(snapshot);
		root_subvol->v.inode	= cpu_to_le64(inum);
		bch2_subvolume_state_set(&root_subvol->v, SUBVOLUME_STATE_live);
		try(bch2_btree_insert_trans(trans, BTREE_ID_subvolumes, &root_subvol->k_i, 0));
	}

	struct bch_inode_unpacked root_inode;
	ret = bch2_inode_find_by_inum_snapshot(trans, BCACHEFS_ROOT_INO, snapshot,
					       &root_inode, 0);
	if (ret && !bch2_err_matches(ret, ENOENT))
		return ret;

	if (mustfix_fsck_err_on(ret,
				trans, root_dir_missing,
				"root directory missing")) {
		bch2_inode_init(c, &root_inode, 0, 0, S_IFDIR|0755,
				0, NULL);
		root_inode.bi_inum = inum;
		root_inode.bi_snapshot = snapshot;

		ret = __bch2_fsck_write_inode(trans, &root_inode);
		bch_err_msg(c, ret, "writing root inode");
	} else if (mustfix_fsck_err_on(!S_ISDIR(root_inode.bi_mode),
				trans, root_inode_not_dir,
				"root inode not a directory")) {
		/*
		 * The inode exists: fix only the mode. Reinitializing it
		 * generates a fresh hash_seed - invalidating the hash offset
		 * of every dirent in the root directory - and wipes bi_subvol:
		 */
		root_inode.bi_mode = S_IFDIR|0755;

		ret = __bch2_fsck_write_inode(trans, &root_inode);
		bch_err_msg(c, ret, "writing root inode");
	}
fsck_err:
	return ret;
}

/* Get root directory, create if it doesn't exist: */
int bch2_check_root(struct bch_fs *c)
{
	CLASS(btree_trans, trans)(c);
	return commit_do(trans, NULL, NULL, BCH_TRANS_COMMIT_no_enospc,
			 check_root_trans(trans));
}

static int fix_reflink_p_key(struct btree_trans *trans, struct btree_iter *iter,
			     struct bkey_s_c k)
{
	struct bkey_s_c_reflink_p p;

	if (k.k->type != KEY_TYPE_reflink_p)
		return 0;

	p = bkey_s_c_to_reflink_p(k);

	if (!p.v->front_pad && !p.v->back_pad)
		return 0;

	struct bkey_i_reflink_p *u = errptr_try(bch2_trans_kmalloc(trans, sizeof(*u)));

	bkey_reassemble(&u->k_i, k);
	u->v.front_pad	= 0;
	u->v.back_pad	= 0;

	return bch2_trans_update(trans, iter, &u->k_i, BTREE_TRIGGER_norun);
}

int bch2_fix_reflink_p(struct bch_fs *c)
{
	if (c->sb.version >= bcachefs_metadata_version_reflink_p_fix)
		return 0;

	CLASS(btree_trans, trans)(c);
	return for_each_btree_key_commit(trans, iter,
				BTREE_ID_extents, POS_MIN,
				BTREE_ITER_intent|BTREE_ITER_prefetch|
				BTREE_ITER_all_snapshots, k,
				NULL, NULL, BCH_TRANS_COMMIT_no_enospc,
			fix_reflink_p_key(trans, &iter, k));
}

/* translate to return code of fsck commad - man(8) fsck */
int bch2_fs_fsck_errcode(struct bch_fs *c, struct printbuf *msg)
{
	int ret = 0;

	if (test_bit(BCH_FS_errors_fixed, &c->flags)) {
		prt_printf(msg, "%s: errors fixed\n", c->name);
		ret |= 1;
	}
	if (test_bit(BCH_FS_error, &c->flags)) {
		prt_printf(msg, "%s: still has errors\n", c->name);
		ret |= 4;
	}
	if (test_bit(BCH_FS_emergency_ro, &c->flags)) {
		prt_printf(msg, "%s: fatal error (went emergency read-only)\n", c->name);
		ret |= 8;
	}

	return ret;
}

#ifndef NO_BCACHEFS_CHARDEV

struct fsck_thread {
	struct thread_with_stdio thr;
	struct bch_fs		*c;
	struct bch_opts		opts;
};

static void bch2_fsck_thread_exit(struct thread_with_stdio *_thr)
{
	struct fsck_thread *thr = container_of(_thr, struct fsck_thread, thr);
	kfree(thr);
}

static int bch2_fsck_offline_thread_fn(struct thread_with_stdio *stdio)
{
	struct fsck_thread *thr = container_of(stdio, struct fsck_thread, thr);
	struct bch_fs *c = thr->c;

	errptr_try(c);

	c->recovery_task = current;

	int ret = bch2_fs_start(c);

	CLASS(printbuf, buf)();
	if (ret) {
		prt_printf(&buf, "%s: error starting filesystem: %s\n", c->name, bch2_err_str(ret));
		/*
		 * What we return is an fsck(8) exit status, not an errcode -
		 * see bch2_fs_fsck_errcode(). A filesystem we couldn't start
		 * is an operational error, same as the online path reports
		 * for recovery passes that fail outright.
		 */
		ret = 8;
	} else
		ret = bch2_fs_fsck_errcode(c, &buf);
	if (ret)
		bch2_stdio_redirect_write(&stdio->stdio, false, buf.buf, buf.pos);

	bch2_fs_exit(c);
	return ret;
}

static const struct thread_with_stdio_ops bch2_offline_fsck_ops = {
	.exit		= bch2_fsck_thread_exit,
	.fn		= bch2_fsck_offline_thread_fn,
};

static int parse_mount_opts_user(char __user *optstr_user, struct bch_opts *opts)
{
	char *optstr __free(kfree) = errptr_try(strndup_user(optstr_user, 1 << 16));

	return bch2_parse_mount_opts(NULL, opts, NULL, optstr, false);
}

long bch2_ioctl_fsck_offline(struct bch_ioctl_fsck_offline __user *user_arg)
{
	struct bch_ioctl_fsck_offline arg;

	try(copy_from_user_errcode(&arg, user_arg, sizeof(arg)));

	if (arg.flags)
		return -BCH_ERR_EINVAL_fsck_offline_bad_flags;

	if (!capable(CAP_SYS_ADMIN))
		return -EPERM;

	struct bch_opts opts = bch2_opts_empty();
	if (arg.opts)
		try(parse_mount_opts_user((char __user *)(unsigned long) arg.opts, &opts));

	CLASS(darray_const_str, devs)();
	for (size_t i = 0; i < arg.nr_devs; i++) {
		u64 dev_u64;
		try(copy_from_user_errcode(&dev_u64, &user_arg->devs[i], sizeof(u64)));

		char *dev_str =
			errptr_try(strndup_user((char __user *)(unsigned long) dev_u64, PATH_MAX));

		int ret = darray_push(&devs, dev_str);
		if (ret) {
			kfree(dev_str);
			return ret;
		}
	}

	struct fsck_thread *thr = kzalloc(sizeof(*thr), GFP_KERNEL);
	if (!thr)
		return -ENOMEM;

	thr->opts = opts;

	opt_set(thr->opts, stdio, (u64)(unsigned long)&thr->thr.stdio);
	opt_set(thr->opts, read_only, 1);
	opt_set(thr->opts, ratelimit_errors, 0);

	/* We need request_key() to be called before we punt to kthread: */
	opt_set(thr->opts, nostart, true);

	bch2_thread_with_stdio_init(&thr->thr, &bch2_offline_fsck_ops);

	thr->c = bch2_fs_open(&devs, &thr->opts, NULL);

	if (!IS_ERR(thr->c) &&
	    thr->c->opts.errors == BCH_ON_ERROR_panic)
		thr->c->opts.errors = BCH_ON_ERROR_ro;

	int ret = __bch2_run_thread_with_stdio(&thr->thr);
	if (ret < 0) {
		if (thr)
			bch2_fsck_thread_exit(&thr->thr);
		pr_err("ret %s", bch2_err_str(ret));
	}
	return ret;
}

static int bch2_fsck_online_thread_fn(struct thread_with_stdio *stdio)
{
	struct fsck_thread *thr = container_of(stdio, struct fsck_thread, thr);
	struct bch_fs *c = thr->c;
	CLASS(printbuf, buf)();
	int ret = -EAGAIN;

	u64 online = bch2_recovery_passes_match(PASS_ONLINE);
	u64 passes = bch2_recovery_passes_match(PASS_FSCK) & online;

	if (opt_defined(thr->opts, recovery_passes)) {
		passes = thr->opts.recovery_passes;

		if ((passes & online) != passes) {
			prt_printf(&buf, "Cannot run passes ");
			prt_bitflags(&buf, bch2_recovery_passes, passes & ~online);
			prt_printf(&buf, " online\n");
			bch2_stdio_redirect_write(&stdio->stdio, false, buf.buf, buf.pos);
			return bch_err_throw(c, EINVAL_fsck_online_bad_passes);
		}
	}

	if (mutex_trylock(&c->recovery.run_lock)) {
		c->stdio_filter = current;
		c->stdio = &thr->thr.stdio;

		/*
		 * XXX: can we figure out a way to do this without mucking with c->opts?
		 */
		unsigned old_fix_errors = c->opts.fix_errors;
		if (opt_defined(thr->opts, fix_errors))
			c->opts.fix_errors = thr->opts.fix_errors;
		else
			c->opts.fix_errors = FSCK_FIX_ask;

		c->opts.fsck = true;
		set_bit(BCH_FS_in_fsck, &c->flags);

		ret = bch2_run_recovery_passes(c, passes, true) ?:
			bch2_fs_fsck_errcode(c, &buf);

		clear_bit(BCH_FS_in_fsck, &c->flags);

		c->stdio = NULL;
		c->stdio_filter = NULL;
		c->opts.fix_errors = old_fix_errors;

		mutex_unlock(&c->recovery.run_lock);
	}
	bch2_ro_ref_put(c);

	if (ret < 0) {
		prt_printf(&buf, "%s: error running recovery passes: %s\n", c->name, bch2_err_str(ret));
		ret = 8;
	}

	if (buf.pos)
		bch2_stdio_redirect_write(&stdio->stdio, false, buf.buf, buf.pos);
	return ret;
}

static const struct thread_with_stdio_ops bch2_online_fsck_ops = {
	.exit		= bch2_fsck_thread_exit,
	.fn		= bch2_fsck_online_thread_fn,
};

long bch2_ioctl_fsck_online(struct bch_fs *c, struct bch_ioctl_fsck_online arg)
{
	if (arg.flags)
		return bch_err_throw(c, EINVAL_fsck_online_bad_flags);

	if (!capable(CAP_SYS_ADMIN))
		return bch_err_throw(c, EPERM_non_admin);

	struct bch_opts opts = bch2_opts_empty();
	if (arg.opts)
		try(parse_mount_opts_user((char __user *)(unsigned long) arg.opts, &opts));

	if (!bch2_ro_ref_tryget(c))
		return -EROFS;

	struct fsck_thread *thr = kzalloc(sizeof(*thr), GFP_KERNEL);
	if (!thr) {
		bch2_ro_ref_put(c);
		return -ENOMEM;
	}

	thr->c = c;
	thr->opts = opts;

	int ret = bch2_run_thread_with_stdio(&thr->thr, &bch2_online_fsck_ops);
	if (ret < 0) {
		bch_err_fn(c, ret);
		bch2_fsck_thread_exit(&thr->thr);
		bch2_ro_ref_put(c);
	}
	return ret;
}

#endif /* NO_BCACHEFS_CHARDEV */
