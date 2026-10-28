/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_FS_H
#define _BCACHEFS_FS_H

#include "fs/inode.h"
#include "fs/str_hash.h"
#include "fs/quota_gen.h"

#include "util/two_state_shared_lock.h"
#include "vfs/types_gen.h"

#include <linux/seqlock.h>
#include <linux/stat.h>
#include <linux/version.h>

#if !defined(__KERNEL__) || LINUX_VERSION_CODE < KERNEL_VERSION(6,19,0)
static inline unsigned inode_state_read(struct inode *inode)
{
	return inode->i_state;
}

static inline unsigned inode_state_read_once(struct inode *inode)
{
	return READ_ONCE(inode->i_state);
}

static inline void inode_state_set_raw(struct inode *inode, unsigned flags)
{
	WRITE_ONCE(inode->i_state, inode->i_state|flags);
}
#endif

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
	return inode->ei_inum_hash.inum;
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

struct bch_inode_info *
__bch2_create(struct mnt_idmap *, struct bch_inode_info *,
	      struct dentry *, umode_t, dev_t, subvol_inum, unsigned);

#if IS_ENABLED(CONFIG_UNICODE)
void bch2_dentry_set_casefold_ops(struct dentry *, struct inode *);
void bch2_dir_casefold_changed(struct dentry *);
#else
static inline void bch2_dentry_set_casefold_ops(struct dentry *dentry, struct inode *vinode) {}
static inline void bch2_dir_casefold_changed(struct dentry *dentry) {}
#endif

#include "vfs/attr_gen.h"
#include "vfs/dir_gen.h"
#include "vfs/export_gen.h"
#include "vfs/inode_gen.h"
#include "vfs/inode_ops_gen.h"
#include "vfs/namei_gen.h"
#include "vfs/super_block_gen.h"

struct bch_inode_info *bch2_vfs_inode_get_trans(struct btree_trans *, subvol_inum, const char *);

/* For Rust's lookup, until the inode cache is Rust: */
struct bch_inode_info *bch2_inode_hash_find(struct bch_fs *, struct btree_trans *, subvol_inum);
struct bch_inode_info *bch2_inode_hash_init_insert(struct btree_trans *, subvol_inum,
						   struct bch_inode_unpacked *,
						   struct bch_subvolume *);
struct inode *bch2_vfs_inode_get(struct bch_fs *, subvol_inum, const char *);

/* For Rust's super ops, until the inode cache is Rust: */
void bch2_evict_inode(struct inode *);
void bch2_free_inode(struct inode *);

void bch2_fs_vfs_exit(struct bch_fs *);
int bch2_fs_vfs_init(struct bch_fs *);
int bch2_fs_vfs_init_rw(struct bch_fs *);

void bch2_vfs_exit(void);
int bch2_vfs_init(void);

static inline void bch2_dirty_inode(struct bch_fs *c, struct bch_inode_info *inode)
{
	if (c->opts.writeback_timeout)
		queue_delayed_work(c->vfs.writeback_wq, &inode->ei_writeback_timer,
				   c->opts.writeback_timeout * HZ);
}

#else

#define bch2_inode_update_after_write(_trans, _inode, _inode_u, _fields)	({ do {} while (0); })

static inline int bch2_inode_or_descendents_is_open(struct btree_trans *trans, struct bpos p) { return 0; }

static inline void bch2_evict_subvolume_inodes(struct bch_fs *c,
					       snapshot_id_list *s) {}

static inline void bch2_fs_vfs_exit(struct bch_fs *c) {}
static inline int bch2_fs_vfs_init(struct bch_fs *c) { return 0; }
static inline int bch2_fs_vfs_init_rw(struct bch_fs *c) { return 0; }

static inline void bch2_vfs_exit(void) {}
static inline int bch2_vfs_init(void) { return 0; }

#endif /* NO_BCACHEFS_FS */

#endif /* _BCACHEFS_FS_H */
