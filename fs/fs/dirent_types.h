/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_FS_DIRENT_TYPES_H
#define _BCACHEFS_FS_DIRENT_TYPES_H

#include "enum_kind.h"

struct qstr;

struct file;

struct dir_context;

struct bch_fs;

struct bch_hash_info;

struct bch_inode_info;

enum __enum_closed bch_rename_mode {
	BCH_RENAME,
	BCH_RENAME_OVERWRITE,
	BCH_RENAME_EXCHANGE,
};

#endif /* _BCACHEFS_FS_DIRENT_TYPES_H */
