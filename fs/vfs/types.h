/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_VFS_TYPES_H
#define _BCACHEFS_VFS_TYPES_H

#include <linux/mempool.h>
#include <linux/rhashtable.h>

#include "snapshots/types.h"
#include "util/fast_list.h"

/*
 * An inode number that something in memory is holding: membership in
 * @inodes_by_inum_table below.
 *
 * bch2_inode_or_descendents_is_open() consults that table and nothing else, so
 * that fsck won't delete an unlinked inode out from under a live reference.
 * Membership - not the existence of a bch_inode_info - is therefore what
 * matters, and this is a standalone type so an entry can be held without one:
 * an unlinked on-disk inode becomes visible to a scanning fsck pass at
 * bch2_trans_commit(), which for O_TMPFILE is before __bch2_create() has a VFS
 * inode to hash.
 *
 * @inum is kept whole rather than split into its fields because
 * @inodes_table keys on the entire subvol_inum.
 */
struct bch_inum_hash_entry {
	struct rhlist_head	hash;
	subvol_inum		inum;
};

struct bch_fs_vfs {
	struct fast_list	inodes;
	struct rhashtable	inodes_table;
	/*
	 * Keyed on the inode number alone, not (subvol, inum): a query asks
	 * whether any snapshot version of an inode number is held, so every
	 * version has to land in one bucket. Hence an rhltable - the key is
	 * deliberately non-unique.
	 */
	struct rhltable		inodes_by_inum_table;

	struct bio_set		writepage_bioset;
	struct bio_set		dio_write_bioset;
	struct bio_set		dio_read_bioset;
	struct bio_set		nocow_flush_bioset;
	mempool_t		writepage_buf_pool;
	struct workqueue_struct	*writeback_wq;
};

#endif /* _BCACHEFS_VFS_TYPES_H */
