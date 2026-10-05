/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_FSCK_H
#define _BCACHEFS_FSCK_H

#include "str_hash.h"

/*
 * snapshots_seen and the inode walker are Rust now (check.rs): C only passes
 * a struct snapshots_seen * through, to __bch2_str_hash_check_key().
 */

int bch2_reconstruct_inode(struct btree_trans *, enum btree_id, u32, u64);
bool bch2_inode_should_reattach(struct bch_inode_unpacked *);

void bch2_dirent_inode_mismatch_msg(struct printbuf *, struct bch_fs *,
				    struct bkey_s_c_dirent,
				    struct bch_inode_unpacked *);

int bch2_reattach_inode(struct btree_trans *, struct bch_inode_unpacked *);

/*
 * Recreate a missing subvolume key: (snapshot, subvol, root inum). Pass 0 for
 * the inum to have it found from the inode carrying bi_subvol.
 *
 * The snapshot must be a leaf - the key it writes sets that snapshot's subvol
 * backref, and bch2_snapshot_validate() rejects a subvol on a node with
 * children. check_snapshots() is the natural caller for that reason: the
 * snapshot it's holding claims the subvolume, so it's a leaf by construction.
 */
int bch2_reconstruct_subvol(struct btree_trans *, u32, u32, u64);

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
