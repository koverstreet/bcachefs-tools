/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_FS_H
#define _BCACHEFS_FS_H

#include "fs/inode.h"
#include "btree/iter.h"
#include "btree/update.h"
#include "data/checksum.h"
#include "init/error.h"
#include "data_structures/darray.h"
#include "snapshots/types_gen.h"
#include "snapshots/subvolume_gen.h"
#include "util/siphash_gen.h"
#include "fs/str_hash_gen.h"
#include "fs/quota_gen.h"

#include "util/two_state_shared_lock_gen.h"
#include "vfs/types_gen.h"

#include <linux/seqlock.h>
#include <linux/stat.h>
#include <linux/version.h>

/* fileattr's struct, before 6.17: the Rust names it file_kattr too */
#if defined(__KERNEL__) && LINUX_VERSION_CODE < KERNEL_VERSION(6,17,0)
#define file_kattr fileattr
#endif

#include "vfs/fs_gen.h"

#define bch2_pagecache_add_put(i)	bch2_two_state_unlock(&(i)->ei_pagecache_lock, 0)
#define bch2_pagecache_add_tryget(i)	bch2_two_state_trylock(&(i)->ei_pagecache_lock, 0)
#define bch2_pagecache_add_get(i)	bch2_two_state_lock(&(i)->ei_pagecache_lock, 0)

#define bch2_pagecache_block_put(i)	bch2_two_state_unlock(&(i)->ei_pagecache_lock, 1)
#define bch2_pagecache_block_get(i)	bch2_two_state_lock(&(i)->ei_pagecache_lock, 1)

DEFINE_GUARD(bch2_pagecache_add, struct bch_inode_info *,
	     bch2_pagecache_add_get(_T),
	     bch2_pagecache_add_put(_T));

DEFINE_GUARD(bch2_pagecache_block, struct bch_inode_info *,
	     bch2_pagecache_block_get(_T),
	     bch2_pagecache_block_put(_T));

static inline subvol_inum inode_inum(const struct bch_inode_info *inode)
{
	return inode->ei_inum;
}

#define to_bch_ei(_inode)					\
	container_of_or_null(_inode, struct bch_inode_info, v)

static inline int ptrcmp(void *l, void *r)
{
	return cmp_int(l, r);
}

#define bch2_lock_inodes(_locks, ...)					\
do {									\
	struct bch_inode_info *a[] = { NULL, __VA_ARGS__ };		\
	unsigned i;							\
									\
	bubble_sort(&a[1], ARRAY_SIZE(a) - 1, ptrcmp);			\
									\
	for (i = 1; i < ARRAY_SIZE(a); i++)				\
		if (a[i] != a[i - 1]) {					\
			if ((_locks) & INODE_PAGECACHE_BLOCK)		\
				bch2_pagecache_block_get(a[i]);\
			if ((_locks) & INODE_UPDATE_LOCK)			\
				mutex_lock_nested(&a[i]->ei_update_lock, i);\
		}							\
} while (0)

#define bch2_unlock_inodes(_locks, ...)					\
do {									\
	struct bch_inode_info *a[] = { NULL, __VA_ARGS__ };		\
	unsigned i;							\
									\
	bubble_sort(&a[1], ARRAY_SIZE(a) - 1, ptrcmp);			\
									\
	for (i = 1; i < ARRAY_SIZE(a); i++)				\
		if (a[i] != a[i - 1]) {					\
			if ((_locks) & INODE_PAGECACHE_BLOCK)		\
				bch2_pagecache_block_put(a[i]);\
			if ((_locks) & INODE_UPDATE_LOCK)			\
				mutex_unlock(&a[i]->ei_update_lock);	\
		}							\
} while (0)

static inline struct bch_inode_info *file_bch_inode(struct file *file)
{
	return to_bch_ei(file_inode(file));
}

#ifndef NO_BCACHEFS_FS

#include "vfs/dir_gen.h"
#include "vfs/file_gen.h"
#include "vfs/inode_ops_gen.h"
#include "vfs/namei_gen.h"

static inline void bch2_dirty_inode(struct bch_fs *c, struct bch_inode_info *inode)
{
	if (c->opts.writeback_timeout)
		queue_delayed_work(c->vfs.writeback_wq, &inode->ei_writeback_timer,
				   c->opts.writeback_timeout * HZ);
}

#endif /* NO_BCACHEFS_FS */

#endif /* _BCACHEFS_FS_H */
