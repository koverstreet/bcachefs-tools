/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_NAMEI_H
#define _BCACHEFS_NAMEI_H

#include "dirent.h"
#include "fs/namei_gen.h"

/*
 * Does linking this inode into a directory bump that directory's i_nlink?
 *
 * A subvolume root doesn't: it's named by a DT_SUBVOL dirent, and those don't
 * count towards the parent's link count. Anything creating or repairing such a
 * link has to agree with the create path here, or check_nlinks() disagrees with
 * it afterwards.
 */
static inline int is_subdir_for_nlink(struct bch_inode_unpacked *inode)
{
	return S_ISDIR(inode->bi_mode) && !inode->bi_subvol;
}

#define INUM_TO_PATH_FAIL_ON_ERR	(1 << 0)

static inline int dirent_points_to_inode_nowarn(struct bch_fs *c,
						struct bkey_s_c_dirent d,
						struct bch_inode_unpacked *inode)
{
	if (d.v->d_type == DT_SUBVOL
	    ? le32_to_cpu(d.v->d_child_subvol)	== inode->bi_subvol
	    : le64_to_cpu(d.v->d_inum)		== inode->bi_inum)
		return 0;
	return bch_err_throw(c, ENOENT_dirent_doesnt_match_inode);
}

static inline bool inode_points_to_dirent(struct bch_inode_unpacked *inode,
					  struct bkey_s_c_dirent d)
{
	return  inode->bi_dir		== d.k->p.inode &&
		inode->bi_dir_offset	== d.k->p.offset;
}

static inline int bch2_check_dirent_target(struct btree_trans *trans,
					   struct btree_iter *dirent_iter,
					   struct bkey_s_c_dirent d,
					   struct bch_inode_unpacked *target,
					   bool in_fsck)
{
	if (likely(inode_points_to_dirent(target, d) &&
		   d.v->d_type == inode_d_type(target)))
		return 0;

	return __bch2_check_dirent_target(trans, dirent_iter, d, target, in_fsck);
}

#endif /* _BCACHEFS_NAMEI_H */
