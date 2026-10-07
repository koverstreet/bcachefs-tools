/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_FS_ACL_TYPES_H
#define _BCACHEFS_FS_ACL_TYPES_H

struct bch_inode_unpacked;

struct bch_hash_info;

struct bch_inode_info;

struct posix_acl;

typedef struct {
	__le16		e_tag;
	__le16		e_perm;
	__le32		e_id;
} bch_acl_entry;

typedef struct {
	__le16		e_tag;
	__le16		e_perm;
} bch_acl_entry_short;

typedef struct {
	__le32		a_version;
} bch_acl_header;

#endif /* _BCACHEFS_FS_ACL_TYPES_H */
