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

#endif /* _BCACHEFS_NAMEI_H */
