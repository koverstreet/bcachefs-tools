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

bool bch2_inode_should_reattach(struct bch_inode_unpacked *inode)
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
			 * before bch2_inode_should_reattach() - having a
			 * backpointer, it would fall into the whiteout arm
			 * and turn the committed fixup into a dangling
			 * backpointer:
			 */
			if (child_inode.bi_dir == inode->bi_dir &&
			    child_inode.bi_dir_offset == inode->bi_dir_offset)
				continue;

			if (!bch2_inode_should_reattach(&child_inode)) {
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

int bch2_reconstruct_inode(struct btree_trans *trans, enum btree_id btree, u32 snapshot, u64 inum)
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
