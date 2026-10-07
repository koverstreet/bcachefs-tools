/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_FS_XATTR_TYPES_H
#define _BCACHEFS_FS_XATTR_TYPES_H

struct xattr_search_key {
	u8		type;
	struct qstr	name;
};

struct dentry;

struct xattr_handler;

struct bch_hash_info;

struct bch_inode_info;

#ifndef NO_BCACHEFS_FS

/* The handlers - fs/xattr.rs - for xattr.c's tables: */
struct mnt_idmap;

struct inode;

#endif

#endif /* _BCACHEFS_FS_XATTR_TYPES_H */
