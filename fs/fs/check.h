/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_FSCK_H
#define _BCACHEFS_FSCK_H

#include "str_hash.h"

/* recoverds snapshot IDs of overwrites at @pos */
struct snapshots_seen {
	struct bpos			pos;
	snapshot_id_list		ids;
};

static inline void snapshots_seen_exit(struct snapshots_seen *s)
{
	darray_exit(&s->ids);
}

static inline struct snapshots_seen snapshots_seen_init(void)
{
	return (struct snapshots_seen) {};
}

DEFINE_CLASS(snapshots_seen, struct snapshots_seen,
	     snapshots_seen_exit(&_T),
	     snapshots_seen_init(), void)

int bch2_snapshots_seen_update(struct bch_fs *, struct snapshots_seen *,
			       enum btree_id, struct bpos);

bool bch2_key_visible_in_snapshot(struct btree_trans *, struct snapshots_seen *, u32, u32);

bool bch2_ref_visible(struct btree_trans *, struct snapshots_seen *, u32, u32);
int bch2_ref_visible2(struct btree_trans *,
		      u32, struct snapshots_seen *,
		      u32, struct snapshots_seen *);

struct inode_walker_entry {
	struct bch_inode_unpacked inode;
	bool			whiteout;
	u64			count;
};

struct inode_walker {
	bool				first_this_inode;
	bool				have_inodes;
	bool				recalculate_sums;
	struct bpos			last_pos;
	/* cached inodes are valid while trans->commit_count is unchanged: */
	u32				commit_count;

	/*
	 * check_key_has_snapshot may migrate a key to a lower snapshot ID -
	 * a position we've already scanned past - so once we've finished the
	 * inode we re-scan it to rebuild the per-inode accumulations. repaired_inum
	 * is the inode that was repaired; restarted_inum bounds this to one
	 * re-scan per inode so it can't loop.
	 */
	u64				repaired_inum;
	u64				restarted_inum;

	DARRAY(struct inode_walker_entry) inodes;
	snapshot_id_list		deletes;
};

static inline void inode_walker_exit(struct inode_walker *w)
{
	darray_exit(&w->inodes);
	darray_exit(&w->deletes);
}

static inline struct inode_walker inode_walker_init(void)
{
	return (struct inode_walker) {};
}

DEFINE_CLASS(inode_walker, struct inode_walker,
	     inode_walker_exit(&_T),
	     inode_walker_init(), void)

struct inode_walker_entry *bch2_walk_inode(struct btree_trans *,
					   struct inode_walker *,
					   struct bkey_s_c);

static inline struct inode_walker_entry *
bch2_visible_inode_next(struct btree_trans *trans, struct snapshots_seen *s,
			struct inode_walker *w, u32 snapshot,
			struct inode_walker_entry *prev)
{
	/*
	 * A version at @snapshot or the first visible ancestor resolves the
	 * ref - possibly a whiteout - and shadows everything past it:
	 */
	if (prev && prev->inode.bi_snapshot >= snapshot)
		return NULL;

	for (struct inode_walker_entry *i = prev ? prev + 1 : w->inodes.data;
	     i < w->inodes.data + w->inodes.nr;
	     i++)
		if (bch2_ref_visible(trans, s, snapshot, i->inode.bi_snapshot))
			return i;
	return NULL;
}

/*
 * Iterate the inode versions whose view includes a key at @_snapshot: every
 * newer version that sees it via the overwrite check, then the one version the
 * key resolves to.
 */
#define for_each_visible_inode(_trans, _s, _w, _snapshot, _i)			\
	for (_i = bch2_visible_inode_next(_trans, _s, _w, _snapshot, NULL);	\
	     _i;								\
	     _i = bch2_visible_inode_next(_trans, _s, _w, _snapshot, _i))

void bch2_dirent_inode_mismatch_msg(struct printbuf *, struct bch_fs *,
				    struct bkey_s_c_dirent,
				    struct bch_inode_unpacked *);

int bch2_reattach_inode(struct btree_trans *, struct bch_inode_unpacked *);

int bch2_fsck_update_backpointers(struct btree_trans *,
				  struct snapshots_seen *,
				  const struct bch_hash_desc,
				  struct bch_hash_info *,
				  struct bkey_i *);

int bch2_check_key_has_inode(struct btree_trans *,
			     struct btree_iter *,
			     struct inode_walker *,
			     struct inode_walker_entry *,
			     struct bkey_s_c);

int bch2_check_inodes(struct bch_fs *);
int bch2_check_extents(struct bch_fs *);
int bch2_check_indirect_extents(struct bch_fs *);
int bch2_check_dirents(struct bch_fs *);
int bch2_check_xattrs(struct bch_fs *);
int bch2_check_root(struct bch_fs *);
int bch2_check_subvolume_structure(struct bch_fs *);
int bch2_check_unreachable_inodes(struct bch_fs *);
int bch2_check_directory_structure(struct bch_fs *);
int bch2_check_nlinks(struct bch_fs *);
int bch2_fix_reflink_p(struct bch_fs *);

int bch2_fs_fsck_errcode(struct bch_fs *, struct printbuf *);
long bch2_ioctl_fsck_offline(struct bch_ioctl_fsck_offline __user *);
long bch2_ioctl_fsck_online(struct bch_fs *, struct bch_ioctl_fsck_online);

#endif /* _BCACHEFS_FSCK_H */
