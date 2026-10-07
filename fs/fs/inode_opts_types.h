/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_FS_INODE_OPTS_TYPES_H
#define _BCACHEFS_FS_INODE_OPTS_TYPES_H

struct bch_inode_unpacked;

struct bkey_i_logged_op_inode_opt_propagate;

/*
 * What changing an inode's options leaves for after the commit: an inode
 * option change has to reach existing data (a reconcile scan) and older
 * snapshots of the inode (the propagate logged op, finished after commit).
 */
struct inode_opt_change {
	bool						reconcile_changed;
	struct bkey_i_logged_op_inode_opt_propagate	propagate;
};

struct bch_extent_reconcile;

#endif /* _BCACHEFS_FS_INODE_OPTS_TYPES_H */
