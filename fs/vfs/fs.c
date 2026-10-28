// SPDX-License-Identifier: GPL-2.0
#ifndef NO_BCACHEFS_FS

#include "bcachefs.h"

#include "alloc/accounting.h"
#include "alloc/background.h"
#include "alloc/buckets.h"

#include "btree/update.h"

#include "data/reconcile/work.h"

#include "fs/check.h"
#include "fs/dirent.h"
#include "fs/inode.h"
#include "fs/namei.h"
#include "fs/quota.h"
#include "fs/xattr.h"

#include "init/chardev.h"
#include "init/dev.h"
#include "init/fs.h"
#include "init/passes.h"

#include "journal/journal.h"

#include "snapshots/snapshot.h"


#include "vfs/fs.h"
#include "vfs/buffered.h"
#include "vfs/direct.h"
#include "vfs/pagecache.h"
#include "vfs/status_fd_gen.h"

#include <linux/aio.h>
#include <linux/backing-dev.h>
#include <linux/compat.h>
#include <linux/exportfs.h>
#include <linux/fileattr.h>
#include <linux/fs_context.h>
#include <linux/module.h>
#include <linux/pagemap.h>
#include <linux/posix_acl.h>
#include <linux/random.h>
#include <linux/security.h>
#include <linux/seq_file.h>
#include <linux/siphash.h>
#include <linux/statfs.h>
#include <linux/string.h>
#include <linux/version.h>
#include <linux/xattr.h>

static struct kmem_cache *bch2_inode_cache;

static void bch2_vfs_inode_init(struct btree_trans *, subvol_inum,
				struct bch_inode_info *,
				struct bch_inode_unpacked *,
				struct bch_subvolume *);

static u32 bch2_vfs_inode_hash_fn(const void *data, u32 len, u32 seed)
{
	const subvol_inum *inum = data;
	siphash_key_t k = { .key[0] = seed };

	return siphash_2u64(inum->subvol, inum->inum, &k);
}

static u32 bch2_vfs_inode_obj_hash_fn(const void *data, u32 len, u32 seed)
{
	const struct bch_inode_info *inode = data;

	return bch2_vfs_inode_hash_fn(&inode->ei_inum_hash.inum, sizeof(inode->ei_inum_hash.inum), seed);
}

static int bch2_vfs_inode_cmp_fn(struct rhashtable_compare_arg *arg,
				 const void *obj)
{
	const struct bch_inode_info *inode = obj;
	const subvol_inum *v = arg->key;

	return !subvol_inum_eq(inode_inum(inode), *v);
}

static const struct rhashtable_params bch2_vfs_inodes_params = {
	.head_offset		= offsetof(struct bch_inode_info, hash),
	.key_offset		= offsetof(struct bch_inode_info, ei_inum_hash.inum),
	.key_len		= sizeof(subvol_inum),
	.hashfn			= bch2_vfs_inode_hash_fn,
	.obj_hashfn		= bch2_vfs_inode_obj_hash_fn,
	.obj_cmpfn		= bch2_vfs_inode_cmp_fn,
	.automatic_shrinking	= true,
};

/*
 * Offsets are relative to bch_inum_hash_entry, not bch_inode_info: entries in
 * this table are not required to be embedded in one.
 */
static const struct rhashtable_params bch2_vfs_inodes_by_inum_params = {
	.head_offset		= offsetof(struct bch_inum_hash_entry, hash),
	.key_offset		= offsetof(struct bch_inum_hash_entry, inum.inum),
	.key_len		= sizeof(u64),
	.automatic_shrinking	= true,
};

int bch2_inode_or_descendents_is_open(struct btree_trans *trans, struct bpos p)
{
	struct bch_fs *c = trans->c;
	struct rhltable *ht = &c->vfs.inodes_by_inum_table;
	u64 inum = p.offset;
	CLASS(darray_u32, subvols)();

	if (!test_bit(BCH_FS_started, &c->flags))
		return false;

	darray_init(&subvols);
restart_from_top:

	/*
	 * Tweaked version of __rhashtable_lookup(); we need to get a list of
	 * subvolumes in which the given inode number is open.
	 *
	 * For this to work, we don't include the subvolume ID in the key that
	 * we hash - all inodes with the same inode number regardless of
	 * subvolume will hash to the same slot.
	 *
	 * This will be less than ideal if the same file is ever open
	 * simultaneously in many different snapshots:
	 */
	rcu_read_lock();
	struct rhash_lock_head __rcu *const *bkt;
	struct rhash_head *he;
	unsigned int hash;
	struct bucket_table *tbl = rht_dereference_rcu(ht->ht.tbl, &ht->ht);
restart:
	hash = rht_key_hashfn(&ht->ht, tbl, &inum, bch2_vfs_inodes_by_inum_params);
	bkt = rht_bucket(tbl, hash);
	do {
		struct bch_inum_hash_entry *e;

		rht_for_each_entry_rcu_from(e, he, rht_ptr_rcu(bkt), tbl, hash, hash.rhead) {
			if (e->inum.inum == inum) {
				int ret = darray_push_gfp(&subvols, e->inum.subvol, GFP_NOWAIT);
				if (ret) {
					rcu_read_unlock();
					try(darray_make_room(&subvols, 1));
					subvols.nr = 0;
					goto restart_from_top;
				}
			}
		}
		/* An object might have been moved to a different hash chain,
		 * while we walk along it - better check and retry.
		 */
	} while (he != RHT_NULLS_MARKER(bkt));

	/* Ensure we see any new tables. */
	smp_rmb();

	tbl = rht_dereference_rcu(tbl->future_tbl, &ht->ht);
	if (unlikely(tbl))
		goto restart;
	rcu_read_unlock();

	darray_for_each(subvols, i) {
		u32 snap;
		try(bch2_subvolume_get_snapshot(trans, *i, &snap));
		try(bch2_snapshot_is_ancestor(trans, snap, p.snapshot));
	}

	return 0;
}

static struct bch_inode_info *__bch2_inode_hash_find(struct bch_fs *c, subvol_inum inum)
{
	return rhashtable_lookup_fast(&c->vfs.inodes_table, &inum, bch2_vfs_inodes_params);
}

static void __wait_on_freeing_inode(struct bch_fs *c,
				    struct bch_inode_info *inode,
				    subvol_inum inum)
{
	wait_queue_head_t *wq;
	struct wait_bit_queue_entry wait;

	wq = inode_bit_waitqueue(&wait, &inode->v, __I_NEW);
	prepare_to_wait(wq, &wait.wq_entry, TASK_UNINTERRUPTIBLE);
	spin_unlock(&inode->v.i_lock);

	if (__bch2_inode_hash_find(c, inum) == inode)
		schedule_timeout(HZ * 10);
	finish_wait(wq, &wait.wq_entry);
}

struct bch_inode_info *bch2_inode_hash_find(struct bch_fs *c, struct btree_trans *trans,
					    subvol_inum inum)
{
	struct bch_inode_info *inode;
repeat:
	inode = __bch2_inode_hash_find(c, inum);
	if (inode) {
		spin_lock(&inode->v.i_lock);
		if (!test_bit(EI_INODE_HASHED, &inode->ei_flags)) {
			spin_unlock(&inode->v.i_lock);
			return NULL;
		}
		if (inode_state_read(&inode->v) & (I_FREEING|I_WILL_FREE)) {
			if (!trans) {
				__wait_on_freeing_inode(c, inode, inum);
			} else {
				int ret = drop_locks_do(trans,
						(__wait_on_freeing_inode(c, inode, inum), 0));
				if (ret)
					return ERR_PTR(ret);
			}
			goto repeat;
		}
		__iget(&inode->v);
		spin_unlock(&inode->v.i_lock);
	}

	return inode;
}

static void bch2_inode_hash_remove(struct bch_fs *c, struct bch_inode_info *inode)
{
	bool remove;
	scoped_guard(spinlock, &inode->v.i_lock)
		remove = test_and_clear_bit(EI_INODE_HASHED, &inode->ei_flags);

	if (remove) {
		int ret = rhltable_remove(&c->vfs.inodes_by_inum_table,
					&inode->ei_inum_hash.hash, bch2_vfs_inodes_by_inum_params);
		BUG_ON(ret);

		ret = rhashtable_remove_fast(&c->vfs.inodes_table,
					&inode->hash, bch2_vfs_inodes_params);
		BUG_ON(ret);
		inode->v.i_hash.pprev = NULL;
		/*
		 * This pairs with the bch2_inode_hash_find() ->
		 * __wait_on_freeing_inode() path
		 */
		inode_wake_up_bit(&inode->v, __I_NEW);
	}
}

/*
 * Marks an inode number as held for as long as the guard is in scope, without
 * an inode to hang it on.
 *
 * O_TMPFILE commits an inode that is already unlinked, so from
 * bch2_trans_commit() onwards a scanning fsck pass can see the key - while
 * __bch2_create() is still several steps away from having a hashed VFS inode.
 * bch2_inode_or_descendents_is_open() would answer "not open" for a file the
 * caller is about to be handed a descriptor for, and check_inode() deletes
 * exactly that.
 *
 * The entry is freed through RCU: rhltable_remove() unlinks it but leaves it
 * traversable, so a reader already inside bch2_inode_or_descendents_is_open()
 * may still be walking it.
 */
struct guard_inum_entry {
	struct bch_inum_hash_entry	e;
	struct rcu_head			rcu;
};

struct inum_hash_guard {
	struct bch_fs		*c;
	struct guard_inum_entry	*e;
};

static void inum_hash_guard_exit(struct inum_hash_guard *g)
{
	if (g->e) {
		int ret = rhltable_remove(&g->c->vfs.inodes_by_inum_table,
					  &g->e->e.hash, bch2_vfs_inodes_by_inum_params);
		BUG_ON(ret);
		kfree_rcu(g->e, rcu);
		g->e = NULL;
	}
}

static struct inum_hash_guard inum_hash_guard_init(struct bch_fs *c)
{
	return (struct inum_hash_guard) { .c = c };
}

DEFINE_CLASS(inum_hash_guard, struct inum_hash_guard,
	     inum_hash_guard_exit(&_T),
	     inum_hash_guard_init(c), struct bch_fs *c)

/*
 * Re-arms: the caller's arm point is inside a transaction retry loop, and a
 * restart can allocate a different inode number.
 */
static int inum_hash_guard_set(struct inum_hash_guard *g, subvol_inum inum)
{
	inum_hash_guard_exit(g);

	struct guard_inum_entry *e = kmalloc(sizeof(*e), GFP_NOFS);
	if (!e)
		return -ENOMEM;

	e->e.inum = inum;
	int ret = rhltable_insert(&g->c->vfs.inodes_by_inum_table,
				  &e->e.hash, bch2_vfs_inodes_by_inum_params);
	if (ret) {
		kfree(e);
		return ret;
	}

	g->e = e;
	return 0;
}

static struct bch_inode_info *bch2_inode_hash_insert(struct bch_fs *c,
						     struct btree_trans *trans,
						     struct bch_inode_info *inode)
{
	struct bch_inode_info *old = inode;

	set_bit(EI_INODE_HASHED, &inode->ei_flags);
retry:
	if (unlikely(rhashtable_lookup_insert_key(&c->vfs.inodes_table,
					&inode->ei_inum_hash.inum,
					&inode->hash,
					bch2_vfs_inodes_params))) {
		old = bch2_inode_hash_find(c, trans, inode_inum(inode));
		if (!old)
			goto retry;

		clear_bit(EI_INODE_HASHED, &inode->ei_flags);

		/*
		 * bcachefs doesn't use I_NEW; we have no use for it since we
		 * only insert fully created inodes in the inode hash table. But
		 * discard_new_inode() expects it to be set...
		 */
		inode_state_set_raw(&inode->v, I_NEW);
		/*
		 * We don't want bch2_evict_inode() to delete the inode on disk,
		 * we just raced and had another inode in cache. Normally new
		 * inodes don't have nlink == 0 - except tmpfiles do...
		 */
		set_nlink(&inode->v, 1);
		discard_new_inode(&inode->v);
		return old;
	} else {
		int ret = rhltable_insert(&c->vfs.inodes_by_inum_table,
					  &inode->ei_inum_hash.hash,
					  bch2_vfs_inodes_by_inum_params);
		BUG_ON(ret);

		inode_fake_hash(&inode->v);

		inode_sb_list_add(&inode->v);

		fast_list_set(&c->vfs.inodes, inode->ei_inodes_idx, inode);
		return inode;
	}
}

static void bch2_vfs_writeback_fn(struct work_struct *work)
{
	struct bch_inode_info *inode = container_of(work, struct bch_inode_info, ei_writeback_timer.work);

	if (!igrab(&inode->v))
		return;

	write_inode_now(&inode->v, false);
	iput(&inode->v);
}

static struct bch_inode_info *__bch2_new_inode(struct bch_fs *c, gfp_t gfp)
{
	struct bch_inode_info *inode = alloc_inode_sb(c->vfs_sb, bch2_inode_cache, gfp);
	if (!inode)
		return NULL;

	int idx = fast_list_get_idx(&c->vfs.inodes, gfp);
	if (idx < 0) {
		kmem_cache_free(bch2_inode_cache, inode);
		return NULL;
	}

	inode_init_once(&inode->v);
	spin_lock_init(&inode->ei_reserved_lock);
	inode->ei_reserved_start	= 0;
	inode->ei_reserved_end		= 0;
	mutex_init(&inode->ei_update_lock);
	two_state_lock_init(&inode->ei_pagecache_lock);
	inode->ei_inodes_idx = idx;
	inode->ei_flags = 0;
	mutex_init(&inode->ei_quota_lock);
	memset(&inode->ei_devs_need_flush, 0, sizeof(inode->ei_devs_need_flush));
	INIT_DELAYED_WORK(&inode->ei_writeback_timer, bch2_vfs_writeback_fn);

	if (unlikely(inode_init_always_gfp(c->vfs_sb, &inode->v, gfp))) {
		fast_list_put_idx(&c->vfs.inodes, idx);
		kmem_cache_free(bch2_inode_cache, inode);
		return NULL;
	}

	return inode;
}

/*
 * Allocate a new inode, dropping/retaking btree locks if necessary:
 */
static struct bch_inode_info *bch2_new_inode(struct btree_trans *trans)
{
	struct bch_inode_info *inode = __bch2_new_inode(trans->c, GFP_NOWAIT);

	if (unlikely(!inode)) {
		int ret = drop_locks_do(trans, (inode = __bch2_new_inode(trans->c, GFP_NOIO)) ? 0 : -ENOMEM);
		if (ret && inode) {
			__destroy_inode(&inode->v);
			fast_list_put_idx(&trans->c->vfs.inodes, inode->ei_inodes_idx);
			kmem_cache_free(bch2_inode_cache, inode);
		}
		if (ret)
			return ERR_PTR(ret);
	}

	return inode;
}

struct bch_inode_info *bch2_inode_hash_init_insert(struct btree_trans *trans,
						   subvol_inum inum,
						   struct bch_inode_unpacked *bi,
						   struct bch_subvolume *subvol)
{
	struct bch_inode_info *inode = bch2_new_inode(trans);
	if (IS_ERR(inode))
		return inode;

	bch2_vfs_inode_init(trans, inum, inode, bi, subvol);

	return bch2_inode_hash_insert(trans->c, trans, inode);

}

static inline struct bch_inode_info *
__bch2_vfs_inode_get_trans(struct btree_trans *trans, subvol_inum inum, const char *warn)
{
	struct bch_inode_info *inode;
	struct bch_inode_unpacked inode_u;
	struct bch_subvolume subvol;
	int ret = bch2_subvolume_get(trans, inum.subvol, warn, &subvol) ?:
		bch2_inode_find_by_inum_snapshot2(trans, inum, le32_to_cpu(subvol.snapshot),
						  &inode_u, 0, warn) ?:
		PTR_ERR_OR_ZERO(inode = bch2_inode_hash_init_insert(trans, inum, &inode_u, &subvol));

	return ret ? ERR_PTR(ret) : inode;
}

struct bch_inode_info *bch2_vfs_inode_get_trans(struct btree_trans *trans, subvol_inum inum,
						const char *warn)
{
	return bch2_inode_hash_find(trans->c, trans, inum) ?:
		__bch2_vfs_inode_get_trans(trans, inum, warn);
}

struct inode *bch2_vfs_inode_get(struct bch_fs *c, subvol_inum inum,
				 const char *warn)
{
	struct bch_inode_info *inode = bch2_inode_hash_find(c, NULL, inum);
	if (inode)
		return &inode->v;

	CLASS(btree_trans, trans)(c);

	int ret = lockrestart_do(trans,
		PTR_ERR_OR_ZERO(inode = bch2_vfs_inode_get_trans(trans, inum, warn)));

	return ret ? ERR_PTR(ret) : &inode->v;
}

struct bch2_initxattrs_ctx {
	struct btree_trans	*trans;
	subvol_inum		inum;
	struct bch_hash_info	hash;
};

static int bch2_initxattrs(struct inode *vinode,
			   const struct xattr *xattr_array,
			   void *fs_data)
{
	struct bch2_initxattrs_ctx *ctx = fs_data;
	const struct xattr *xattr;

	for (xattr = xattr_array; xattr->name; xattr++) {
		int ret = __bch2_xattr_set(ctx->trans, ctx->inum, &ctx->hash,
					   xattr->name, xattr->value,
					   xattr->value_len,
					   KEY_TYPE_XATTR_INDEX_SECURITY,
					   XATTR_CREATE);
		if (ret)
			return ret;
	}

	return 0;
}

static int bch2_inode_init_security(struct btree_trans *trans,
				    struct bch_inode_info *dir,
				    struct bch_inode_info *inode,
				    const struct qstr *name,
				    subvol_inum inum,
				    struct bch_inode_unpacked *inode_u,
				    unsigned flags)
{
	struct bch2_initxattrs_ctx ctx = {
		.trans	= trans,
		.inum	= inum,
	};

	/*
	 * A snapshot create doesn't create an inode: bch2_create_trans() looks
	 * up the source subvolume's root and reuses it at the new snapshot ID,
	 * so the source's security xattr is already visible there through
	 * snapshot ancestry. Handing that inode to the LSM would have it
	 * compute a fresh label and write it with XATTR_CREATE, which finds the
	 * inherited key and fails the whole snapshot with EEXIST_str_hash_set.
	 *
	 * A snapshot inherits the source's label, which is also the semantics
	 * we want - the new root is the same inode, not a new object to be
	 * labelled from the calling task. bch2_create_trans() skips the ACLs
	 * for a snapshot for the same reason; ACLs are xattrs too.
	 *
	 * The in-core label still gets set: the new subvolume root is
	 * instantiated after commit, and the LSM's d_instantiate hook reads it
	 * back off the inherited xattr.
	 */
	if (flags & BCH_CREATE_SNAPSHOT)
		return 0;

	/*
	 * The LSM computes the new label from the task, the dir, and the new
	 * inode's mode; the vfs inode isn't initialized until after commit
	 * (bch2_vfs_inode_init), so give it just enough identity here:
	 */
	inode->v.i_mode = inode_u->bi_mode;
	i_uid_write(&inode->v, inode_u->bi_uid);
	i_gid_write(&inode->v, inode_u->bi_gid);

	try(bch2_hash_info_init(trans->c, inode_u, &ctx.hash));

	return security_inode_init_security(&inode->v, &dir->v, name,
					    bch2_initxattrs, &ctx);
}

DEFINE_FREE(posix_acl, struct posix_acl *, posix_acl_release(_T))

struct bch_inode_info *
__bch2_create(struct mnt_idmap *idmap,
	      struct bch_inode_info *dir, struct dentry *dentry,
	      umode_t mode, dev_t rdev, subvol_inum snapshot_src,
	      unsigned flags)
{
	struct bch_fs *c = dir->v.i_sb->s_fs_info;
	struct bch_inode_unpacked dir_u;
	struct bch_inode_info *inode;
	struct bch_inode_unpacked inode_u;
	struct posix_acl *default_acl __free(posix_acl) = NULL;
	struct posix_acl *acl __free(posix_acl) = NULL;
	subvol_inum inum;
	struct bch_subvolume subvol;
	kuid_t kuid = mapped_fsuid(idmap, i_user_ns(&dir->v));
	kgid_t kgid = mapped_fsgid(idmap, i_user_ns(&dir->v));
	bool is_posixacl = IS_POSIXACL(&dir->v);
	int ret;

	/*
	 * preallocate acls + vfs inode before btree transaction, so that
	 * nothing can fail after the transaction succeeds:
	 */
	if (is_posixacl) {
		ret = posix_acl_create(&dir->v, &mode, &default_acl, &acl);
		if (ret)
			return ERR_PTR(ret);
	}

	inode = __bch2_new_inode(c, GFP_NOIO);
	if (unlikely(!inode))
		return ERR_PTR(-ENOMEM);

	bch2_inode_init_early(c, &inode_u);

	if (!(flags & BCH_CREATE_TMPFILE))
		mutex_lock(&dir->ei_update_lock);
	/*
	 * posix_acl_create() calls get_acl -> btree transaction, don't start
	 * ours until after, ei->update_lock must also be taken first:
	 */
	CLASS(btree_trans, trans)(c);
	CLASS(inum_hash_guard, guard)(c);
retry:
	bch2_trans_begin(trans);

	/*
	 * Quotas count only master subvolume inodes; a new subvolume's root is
	 * in a new snapshot tree, and is its master unless it's a snapshot:
	 */
	bool quota = flags & BCH_CREATE_SUBVOL
		? !(flags & BCH_CREATE_SNAPSHOT)
		: !test_bit(EI_INODE_SNAPSHOT, &dir->ei_flags);

	ret   = bch2_create_trans(trans,
				  inode_inum(dir), &dir_u, &inode_u, &subvol,
				  !(flags & BCH_CREATE_TMPFILE)
				  ? &dentry->d_name : NULL,
				  from_kuid(i_user_ns(&dir->v), kuid),
				  from_kgid(i_user_ns(&dir->v), kgid),
				  mode, rdev,
				  default_acl, acl, snapshot_src, flags) ?:
		(quota ? bch2_quota_acct(c, bch_qid(&inode_u), Q_INO, 1,
					 KEY_TYPE_QUOTA_PREALLOC) : 0);
	if (unlikely(ret))
		goto err_before_quota;

	inum.subvol = inode_u.bi_subvol ?: inode_inum(dir).subvol;
	inum.inum = inode_u.bi_inum;

	/*
	 * Before the commit publishes the key. Only a tmpfile needs it: every
	 * other create commits with a dirent and nlink 1, which check_inode()
	 * doesn't touch.
	 */
	ret =   ((flags & BCH_CREATE_TMPFILE)
		 ? inum_hash_guard_set(&guard, inum)
		 : 0) ?:
		bch2_inode_init_security(trans, dir, inode,
					 !(flags & BCH_CREATE_TMPFILE)
					 ? &dentry->d_name : NULL,
					 inum, &inode_u, flags) ?:
		bch2_trans_commit(trans, NULL, NULL, 0);
	if (unlikely(ret)) {
		if (quota)
			bch2_quota_acct(c, bch_qid(&inode_u), Q_INO, -1,
					KEY_TYPE_QUOTA_WARN);
err_before_quota:
		if (bch2_err_matches(ret, BCH_ERR_transaction_restart))
			goto retry;

		if (!(flags & BCH_CREATE_TMPFILE))
			mutex_unlock(&dir->ei_update_lock);

		make_bad_inode(&inode->v);
		iput(&inode->v);
		return ERR_PTR(ret);
	}

	if (!(flags & BCH_CREATE_TMPFILE)) {
		bch2_inode_update_after_write(trans, dir, &dir_u,
					      ATTR_MTIME|ATTR_CTIME|ATTR_SIZE);
		mutex_unlock(&dir->ei_update_lock);
	}

	bch2_vfs_inode_init(trans, inum, inode, &inode_u, &subvol);

	if (is_posixacl) {
		set_cached_acl(&inode->v, ACL_TYPE_ACCESS, acl);
		set_cached_acl(&inode->v, ACL_TYPE_DEFAULT, default_acl);
	}

	/*
	 * we must insert the new inode into the inode cache before calling
	 * bch2_trans_exit() and dropping locks, else we could race with another
	 * thread pulling the inode in and modifying it:
	 *
	 * also, calling bch2_inode_hash_insert() without passing in the
	 * transaction object is sketchy - if we could ever end up in
	 * __wait_on_freeing_inode(), we'd risk deadlock.
	 *
	 * But that shouldn't be possible, since we still have the inode locked
	 * that we just created, and we _really_ can't take a transaction
	 * restart here.
	 */
	return bch2_inode_hash_insert(c, NULL, inode);
}

/* methods */

static const struct address_space_operations bch_address_space_operations = {
	.read_folio	= bch2_read_folio,
	.writepages	= bch2_writepages,
	.readahead	= bch2_readahead,
	.dirty_folio	= bch2_vfs_dirty_folio,
	.write_begin	= bch2_write_begin,
	.write_end	= bch2_write_end,
	.invalidate_folio = bch2_invalidate_folio,
	.release_folio	= bch2_release_folio,
#ifdef CONFIG_MIGRATION
	.migrate_folio	= filemap_migrate_folio,
#endif
	.error_remove_folio = generic_error_remove_folio,
};

static void bch2_vfs_inode_init(struct btree_trans *trans,
				subvol_inum inum,
				struct bch_inode_info *inode,
				struct bch_inode_unpacked *bi,
				struct bch_subvolume *subvol)
{
	inode->v.i_ino		= inum.inum;
	inode->ei_inum_hash.inum = inum;
	inode->ei_inode.bi_inum	= inum.inum;
	bch2_inode_update_after_write(trans, inode, bi, ~0);

	inode->v.i_blocks	= bi->bi_sectors;
	inode->v.i_rdev		= bi->bi_dev;
	inode->v.i_generation	= bi->bi_generation;
	inode->v.i_size		= bi->bi_size;

	inode->ei_flags		= 0;
	inode->ei_quota_reserved = 0;
	inode->ei_qid		= bch_qid(bi);

	if (BCH_SUBVOLUME_SNAP(subvol))
		set_bit(EI_INODE_SNAPSHOT, &inode->ei_flags);

	inode->v.i_mapping->a_ops = &bch_address_space_operations;

	switch (inode->v.i_mode & S_IFMT) {
	case S_IFREG:
		inode->v.i_op	= &bch2_file_inode_ops;
		inode->v.i_fop	= &bch2_file_ops;
		break;
	case S_IFDIR:
		inode->v.i_op	= &bch2_dir_inode_ops;
		inode->v.i_fop	= &bch2_dir_file_ops;
		break;
	case S_IFLNK:
		inode_nohighmem(&inode->v);
		inode->v.i_op	= &bch2_symlink_inode_ops;
		break;
	default:
		init_special_inode(&inode->v, inode->v.i_mode, inode->v.i_rdev);
		inode->v.i_op	= &bch2_special_inode_ops;
		break;
	}

	mapping_set_folio_min_order(inode->v.i_mapping,
				    get_order(trans->c->opts.block_size));
}

void bch2_free_inode(struct inode *vinode)
{
	kmem_cache_free(bch2_inode_cache, to_bch_ei(vinode));
}

#if 0
static bool verify_i_size_at_evict;

module_param_named(verify_i_size_at_evict, verify_i_size_at_evict, bool, 0644);
MODULE_PARM_DESC(verify_i_size_at_evict, "");
#endif

void bch2_evict_inode(struct inode *vinode)
{
	struct bch_fs *c = vinode->i_sb->s_fs_info;
	struct bch_inode_info *inode = to_bch_ei(vinode);
	/*
	 * An unlinked subvolume's root inode reads i_nlink == 0, but its
	 * deletion belongs to the subvolume deletion path, not us (see
	 * bch2_inode_is_subvolume_root()): this eviction completing is what
	 * the subvolume delete work (SubvolDelete, subvolume.rs) is waiting on
	 * to tombstone the subvolume and hand the keys to the snapshot sweep.
	 */
	bool delete = !inode->v.i_nlink && !is_bad_inode(&inode->v) &&
		!bch2_inode_is_subvolume_root(&inode->ei_inode);

	/*
	 * evict() has waited for outstanding writeback, we'll do no more IO
	 * through this inode: it's safe to remove from VFS inode hashtable here
	 *
	 * Do that now so that other threads aren't blocked from pulling it back
	 * in, there's no reason for them to be:
	 */
	if (!delete)
		bch2_inode_hash_remove(c, inode);

	if (IS_ENABLED(CONFIG_BCACHEFS_DEBUG) && delete)
		write_inode_now(&inode->v, true);
	/*
	 * Disabled: bch2_inode_find_by_inum() starts a new btree_trans, but
	 * eviction can be called from inside an existing transaction — e.g.
	 * discard_new_inode() from bch2_inode_hash_insert() during a
	 * lookup. The deadlock detector can't see locks held by the
	 * outer transaction, so cycles involving both transactions
	 * go undetected. Observed as a three-way deadlock between
	 * btree_node_write_work, __bch2_unlink, and bch2_lookup (github
	 * issue #1046).
	 */
#if 0
	if (verify_i_size_at_evict) {
		BUG_ON(inode_state_read_once(&inode->v) & I_DIRTY);

		struct bch_inode_unpacked inode_u;
		if (!is_bad_inode(&inode->v) &&
		    !bch2_journal_error(&c->journal) &&
		    !bch2_inode_find_by_inum(c, inode_inum(inode), &inode_u)) {
			if (inode->v.i_size != inode_u.bi_size) {
				CLASS(bch_log_msg, msg)(c);

				prt_printf(&msg.m, "vfs i_size mismatch at evict: %llu != %llu (inum %llu)\n",
					   (u64) inode->v.i_size, inode_u.bi_size, inode_u.bi_inum);
				bch2_trans_do(c, bch2_inum_to_path(trans, inode_inum(inode), &msg.m));
				prt_newline(&msg.m);
				bch2_inode_unpacked_to_text(&msg.m, &inode_u);
				msg.m.suppress = !bch2_count_fsck_err(c, vfs_i_size_bad, &msg.m);
			}

			if (inode->v.i_blocks != inode_u.bi_sectors) {
				CLASS(bch_log_msg, msg)(c);

				prt_printf(&msg.m, "vfs i_sectors mismatch at evict: %llu != %llu (inum %llu)\n",
					   (u64) inode->v.i_blocks, inode_u.bi_sectors, inode_u.bi_inum);
				bch2_trans_do(c, bch2_inum_to_path(trans, inode_inum(inode), &msg.m));
				prt_newline(&msg.m);
				bch2_inode_unpacked_to_text(&msg.m, &inode_u);
				msg.m.suppress = !bch2_count_fsck_err(c, vfs_i_sectors_bad, &msg.m);
			}
		}
	}
#endif
	truncate_inode_pages_final(&inode->v.i_data);

	cancel_delayed_work_sync(&inode->ei_writeback_timer);

	clear_inode(&inode->v);

	BUG_ON(!is_bad_inode(&inode->v) && inode->ei_quota_reserved);

	if (delete) {
		if (!test_bit(EI_INODE_SNAPSHOT, &inode->ei_flags)) {
			bch2_quota_acct(c, inode->ei_qid, Q_SPC, -((s64) inode->v.i_blocks),
					KEY_TYPE_QUOTA_WARN);
			bch2_quota_acct(c, inode->ei_qid, Q_INO, -1,
					KEY_TYPE_QUOTA_WARN);
		}
		bch2_inode_rm(c, inode_inum(inode));

		/*
		 * If we are deleting, we need it present in the vfs hash table
		 * so that fsck can check if unlinked inodes are still open:
		 */
		bch2_inode_hash_remove(c, inode);
	}

	fast_list_remove(&c->vfs.inodes, inode->ei_inodes_idx);
	inode->ei_inodes_idx = 0;
}

void bch2_evict_subvolume_inodes(struct bch_fs *c, snapshot_id_list *s)
{
	/*
	 * Initially, we scan for inodes without I_DONTCACHE, then mark them to
	 * be pruned with d_mark_dontcache().
	 *
	 * Once we've had a clean pass where we didn't find any inodes without
	 * I_DONTCACHE, we wait for them to be freed.
	 *
	 * fast_list_iter tracks position by integer index, so we can drop rcu
	 * around the dcache work (held safe by our igrab ref) and resume from
	 * the same slot on the next iteration.
	 */
	rcu_read_lock();
	struct genradix_iter iter;
	struct bch_inode_info *inode;

	fast_list_for_each(&c->vfs.inodes, iter, inode) {
		if (!snapshot_list_has_id(s, inode_inum(inode).subvol))
			continue;

		if (!(inode_state_read_once(&inode->v) & I_DONTCACHE) &&
		    igrab(&inode->v)) {
			rcu_read_unlock();

			d_mark_dontcache(&inode->v);
			d_prune_aliases(&inode->v);
			iput(&inode->v);

			rcu_read_lock();
		}
	}

	bool found = false;
	do {
		found = false;

		fast_list_for_each(&c->vfs.inodes, iter, inode) {
			if (!snapshot_list_has_id(s, inode_inum(inode).subvol))
				continue;

			found = true;

			struct wait_bit_queue_entry wqe;
			struct wait_queue_head *wq_head = inode_bit_waitqueue(&wqe, &inode->v, __I_NEW);
			prepare_to_wait_event(wq_head, &wqe.wq_entry,
					      TASK_UNINTERRUPTIBLE);
			rcu_read_unlock();

			schedule();
			finish_wait(wq_head, &wqe.wq_entry);
			rcu_read_lock();
		}
	} while (found);
	rcu_read_unlock();
}

static struct bch_fs *bch2_path_to_fs(const char *path)
{
	struct bch_fs *c;
	dev_t dev;
	int ret;

	ret = lookup_bdev(path, &dev);
	if (ret)
		return ERR_PTR(ret);

	c = bch2_dev_to_fs(dev);
	if (c)
		closure_put(&c->cl);
	return c ?: ERR_PTR(-ENOENT);
}

/*
 * bcachefs doesn't currently integrate intwrite freeze protection but the
 * internal write references serve the same purpose. Therefore reuse the
 * read-only transition code to perform the quiesce. The caveat is that we don't
 * currently have the ability to block tasks that want a write reference while
 * the superblock is frozen. This is fine for now, but we should either add
 * blocking support or find a way to integrate sb_start_intwrite() and friends.
 */
static int bch2_set_super(struct super_block *s, struct fs_context *fc)
{
	s->s_fs_info = fc->s_fs_info;
	return 0;
}

static int bch2_noset_super(struct super_block *s, struct fs_context *fc)
{
	return -EBUSY;
}

typedef DARRAY(struct bch_fs *) darray_fs;

static int bch2_test_super(struct super_block *s, struct fs_context *fc)
{
	struct bch_fs *c = s->s_fs_info;
	darray_fs *d = fc->s_fs_info;

	if (!c)
		return false;

	darray_for_each(*d, i)
		if (c != *i)
			return false;
	return true;
}

static void set_mount_opts(struct bch_fs *c, struct bch_opts *opts)
{
	for (enum bch_opt_id id = 0; id < bch2_opts_nr; id++)
		if (bch2_opt_defined_by_id(opts, id))
			set_bit(id, c->mount_opts.d);
}

static int bch2_fs_get_tree(struct fs_context *fc)
{
	struct bch_fs *c;
	struct super_block *sb;
	struct inode *vinode;
	struct bch2_opts_parse *opts_parse = fc->fs_private;
	struct bch_opts opts = opts_parse->opts;
	darray_fs devs_to_fs = {};
	int ret;

	opt_set(opts, read_only, (fc->sb_flags & SB_RDONLY) != 0);
	opt_set(opts, nostart, true);

	/*
	 * Accumulated by bch2_fs_parse_param() from however many "source"
	 * parameters we were given, and owned by opts_parse - so it outlives
	 * this function and is freed by bch2_fs_context_free().
	 */
	if (!opts_parse->devs.nr)
		return invalf(fc, "no device to mount");

	darray_for_each(opts_parse->devs, i) {
		ret = darray_push(&devs_to_fs, bch2_path_to_fs(*i));
		if (ret)
			goto err;
	}

	/* sget_fc() passes the payload to the callbacks via fc->s_fs_info, and
	 * takes the sb flags from fc->sb_flags (no flags argument). */
	fc->s_fs_info = &devs_to_fs;
	fc->sb_flags |= SB_NOSEC;
	sb = sget_fc(fc, bch2_test_super, bch2_noset_super);
	if (!IS_ERR(sb))
		goto got_sb;

	c = bch2_fs_open(&opts_parse->devs, &opts,
			 opts_parse->user_key_set ? &opts_parse->user_key : NULL);
	ret = PTR_ERR_OR_ZERO(c);
	if (ret)
		goto err;

	if (opt_defined(opts, discard))
		set_bit(BCH_FS_discard_mount_opt_set, &c->flags);
	set_mount_opts(c, &opts);

	/* Some options can't be parsed until after the fs is started: */
	opts = bch2_opts_empty();
	ret = bch2_parse_mount_opts(c, &opts, NULL, opts_parse->parse_later.buf, false);
	if (ret)
		goto err_stop_fs;

	bch2_opts_apply(&c->opts, opts);
	set_mount_opts(c, &opts);

	/*
	 * Both directions of the status channel are scoped to recovery: c->stdio
	 * must not outlive the reference bch2_fs_context_free() drops on the file
	 * it points into, and the channel's filesystem must not outlive @c, which
	 * the error path below is free to tear down.
	 *
	 * stdio_user_only describes that redirect, so it dies with it: an online
	 * fsck later installs its own c->stdio and expects its log traffic to
	 * reach it, and nothing else ever clears this.
	 */
	struct file *status = opts_parse->status_file;

	if (status)
		bch2_status_fd_attach(status, c);

	ret = bch2_fs_start(c);

	if (status)
		bch2_status_fd_detach(status, c);

	if (ret)
		goto err_stop_fs;

	/*
	 * We might be doing a RO mount because other options required it, or we
	 * have no alloc info and it's a small image with no room to regenerate
	 * it
	 */
	if (c->opts.read_only)
		fc->sb_flags |= SB_RDONLY;

	fc->s_fs_info = c;
	sb = sget_fc(fc, NULL, bch2_set_super);
	ret = PTR_ERR_OR_ZERO(sb);
	if (ret)
		goto err_stop_fs;
got_sb:
	c = sb->s_fs_info;

	if (sb->s_root) {
		if ((fc->sb_flags ^ sb->s_flags) & SB_RDONLY) {
			ret = -EBUSY;
			goto err_put_super;
		}
		goto out;
	}

	sb->s_blocksize		= block_bytes(c);
	sb->s_blocksize_bits	= ilog2(block_bytes(c));
	sb->s_maxbytes		= MAX_LFS_FILESIZE;
	sb->s_op		= &bch2_super_ops;
	sb->s_export_op		= &bch2_export_ops;
#ifdef CONFIG_BCACHEFS_QUOTA
	sb->s_qcop		= &bch2_quotactl_operations;
	sb->s_quota_types	= QTYPE_MASK_USR|QTYPE_MASK_GRP|QTYPE_MASK_PRJ;
#endif
	sb->s_xattr		= bch2_xattr_handlers;
	sb->s_magic		= BCACHEFS_STATFS_MAGIC;
	sb->s_time_gran		= c->sb.nsec_per_time_unit;
	sb->s_time_min		= div_s64(S64_MIN, c->sb.time_units_per_sec) + 1;
	sb->s_time_max		= div_s64(S64_MAX, c->sb.time_units_per_sec);
	super_set_uuid(sb, c->sb.user_uuid.b, sizeof(c->sb.user_uuid));

	if (c->sb.multi_device)
		super_set_sysfs_name_uuid(sb);
	else
		strscpy(sb->s_sysfs_name, c->name, sizeof(sb->s_sysfs_name));

	sb->s_shrink->seeks	= 0;
	c->vfs_sb		= sb;
	strscpy(sb->s_id, c->name, sizeof(sb->s_id));

	ret = super_setup_bdi(sb);
	if (ret)
		goto err_put_super;

	sb->s_bdi->ra_pages = bch2_fs_ra_pages(c);

	scoped_guard(rcu) {
		for_each_online_member_rcu(c, ca) {
			struct block_device *bdev = ca->disk_sb.bdev;

			/* XXX: create an anonymous device for multi device filesystems */
			sb->s_bdev	= bdev;
			sb->s_dev	= bdev->bd_dev;
			break;
		}
	}

	c->dev = sb->s_dev;

	if (c->opts.acl)
		sb->s_flags	|= SB_POSIXACL;

	sb->s_shrink->seeks = 0;

#if IS_ENABLED(CONFIG_UNICODE)
	if (!bch2_fs_casefold_enabled(c))
		sb->s_encoding = c->cf_encoding;
	/*
	 * Avoid generic_set_sb_d_ops()'s generic_ci_dentry_ops default:
	 * directory dentries that need casefold support get utf8 ops installed
	 * when their inode is attached (see bch2_dentry_set_casefold_ops),
	 * keeping DCACHE_OP_COMPARE clear on non-casefolded directories.
	 */
#endif

	vinode = bch2_vfs_inode_get(c, BCACHEFS_ROOT_SUBVOL_INUM, __func__);
	ret = PTR_ERR_OR_ZERO(vinode);
	bch_err_msg(c, ret, "mounting: error getting root inode");
	if (ret)
		goto err_put_super;

	sb->s_root = d_make_root(vinode);
	if (!sb->s_root) {
		bch_err(c, "error mounting: error allocating root dentry");
		ret = -ENOMEM;
		goto err_put_super;
	}

#if IS_ENABLED(CONFIG_UNICODE)
	/* Root has no attach site of its own; set its ops from its own inode. */
	bch2_dentry_set_casefold_ops(sb->s_root, d_inode(sb->s_root));
#endif

	sb->s_flags |= SB_ACTIVE;
out:
	fc->root = dget(sb->s_root);
err:
	darray_exit(&devs_to_fs);
	/*
	 * errorfc() rather than pr_err() because logfc() falls back to printk
	 * when there's no fs_context log, so a mount(2) caller still gets this
	 * in dmesg, and an fsconfig(2) one gets it on the terminal.
	 */
	if (ret)
		errorfc(fc, "%s", bch2_err_str(ret));
	/*
	 * On an inconsistency error in recovery we might see an -EROFS derived
	 * errorcode (from the journal), but we don't want to return that to
	 * userspace as that causes util-linux to retry the mount RO - which is
	 * confusing:
	 */
	if (bch2_err_matches(ret, EROFS) && ret != -EROFS)
		ret = -EIO;
	/*
	 * Deliberately not bch2_err_class(): mount.bcachefs has to tell our
	 * errors apart to know what to do about them - whether to put the
	 * degraded question to the user, whether a refusal is worth escalating.
	 * Flattening device_splitbrain and insufficient_devices_to_start both to
	 * EINVAL leaves it nothing to go on but the log text, and prose is not an
	 * interface.
	 *
	 * Safe as a syscall return: an errcode is BCH_ERR_START (2048) plus a
	 * number pinned in the errcode x-macro, and the largest is far below
	 * MAX_ERRNO, so IS_ERR_VALUE() still reads it as an error. A caller that
	 * doesn't know bcachefs gets an unrecognised errno instead of a
	 * plausible wrong one - which is also why the EROFS remap above stays:
	 * that is a case where a caller does act on the errno.
	 */
	return ret;

err_stop_fs:
	bch2_fs_exit(c);
	goto err;

err_put_super:
	if (!sb->s_root)
		bch2_fs_stop(c);
	deactivate_locked_super(sb);
	goto err;
}

static void bch2_kill_sb(struct super_block *sb)
{
	struct bch_fs *c = sb->s_fs_info;

	generic_shutdown_super(sb);
	bch2_fs_exit(c);
}

static void bch2_fs_context_free(struct fs_context *fc)
{
	struct bch2_opts_parse *opts = fc->fs_private;

	if (opts) {
		printbuf_exit(&opts->parse_later);
		darray_exit_free_item(&opts->devs, kfree);
		if (opts->status_file)
			bch2_status_fd_put(opts->status_file);

		memzero_explicit(&opts->user_key, sizeof(opts->user_key));
		kfree(opts);
	}
}

/*
 * Keep fc->source in step with the devices we've been given: taking the
 * "source" parameter ourselves means vfs_parse_fs_param_source() never runs,
 * and fc->source is what /proc/mounts and mountinfo display. It has to end up
 * looking like what a single colon-separated source would have produced. No
 * length limit applies here - fsconfig(2)'s 256-byte cap is on what it will
 * copy in from userspace, not on what we assemble.
 */
static int bch2_fc_source_append(struct fs_context *fc, const char *s)
{
	char *new = fc->source
		? kasprintf(GFP_KERNEL, "%s:%s", fc->source, s)
		: kstrdup(s, GFP_KERNEL);
	if (!new)
		return -ENOMEM;

	kfree(fc->source);
	fc->source = new;
	return 0;
}

static int bch2_fs_parse_param(struct fs_context *fc,
			       struct fs_parameter *param)
{
	struct bch2_opts_parse *opts = fc->fs_private;
	struct bch_fs *c = NULL;

	/*
	 * struct fs_parameter keeps its value in a union and @type says which
	 * member is live, but nothing between fsconfig(2) and here checks that
	 * the two agree - filesystems that go through fs_parse() have it done
	 * against their parameter spec, and we parse by hand. So settle it once,
	 * for every parameter below: a value we can read as a string, or a flag
	 * with no value at all. The rest put a struct filename or a struct file
	 * in that same member, and reading one of those as a string means
	 * strlen() over an object that has no reason to contain a NUL.
	 */
	switch (param->type) {
	case fs_value_is_string:
	case fs_value_is_flag:
		break;
	default:
		return invalf(fc, "%s: expected a string value", param->key);
	}

	/*
	 * The devices to mount. We take this rather than leaving it to the VFS
	 * because fsconfig(2) copies a parameter value with
	 * strndup_user(_value, 256): a long device list can't be passed as one
	 * string, so userspace hands them over a few at a time - and
	 * vfs_parse_fs_param_source() refuses the second one ("Multiple
	 * sources").
	 *
	 * Legacy mount(2) lands here too, via vfs_parse_fs_string(fc, "source",
	 * name), as a single colon-separated value. Splitting each parameter
	 * and appending covers both without a second path.
	 */
	if (!strcmp(param->key, "source")) {
		/*
		 * -EINVAL is an empty device name and nothing else; -ENOMEM is
		 * the only other failure. Naming it here rather than in
		 * bch2_split_devs() is the point of taking this parameter at
		 * all - the value that was wrong is in front of us, and it
		 * reaches the user through the fs_context log.
		 */
		int ret = bch2_split_devs(param->string, &opts->devs);
		if (ret)
			return ret == -EINVAL
				? invalf(fc, "empty device name in source \"%s\"",
					 param->string)
				: ret;

		return bch2_fc_source_append(fc, param->string);
	}

	/*
	 * Not a mount option: mount.bcachefs's channel to the filesystem coming
	 * up. The fd number goes back as this fsconfig(2) call's return value,
	 * which reaches userspace untouched - vfs_parse_fs_param() returns
	 * anything that isn't -ENOPARAM verbatim, and so does
	 * vfs_fsconfig_locked().
	 */
	if (!strcmp(param->key, "status_fd")) {
		if (opts->status_file)
			return invalf(fc, "status_fd requested twice");

		return bch2_status_fd_open(&opts->status_file);
	}

	/*
	 * The passphrase-derived key that unwraps the superblock's, for callers
	 * that already have it. A parameter and not a mount option because
	 * fsconfig(2) copies it out of our address space: it never reaches
	 * anyone's ps output the way an -o would.
	 */
	if (!strcmp(param->key, "user_key")) {
		if (!param->string)
			return invalf(fc, "user_key: no key given");

		size_t len = strlen(param->string);
		int ret = len == sizeof(opts->user_key) * 2
			? hex2bin((u8 *) &opts->user_key, param->string,
				  sizeof(opts->user_key))
			: -EINVAL;

		/* the VFS frees this with kfree(), not kfree_sensitive() */
		memzero_explicit(param->string, len);

		if (ret) {
			memzero_explicit(&opts->user_key, sizeof(opts->user_key));
			return invalf(fc, "user_key: expected %zu hex digits",
				      sizeof(opts->user_key) * 2);
		}

		opts->user_key_set = true;
		return 0;
	}

	/* for reconfigure, we already have a struct bch_fs */
	if (fc->root)
		c = fc->root->d_sb->s_fs_info;

	CLASS(printbuf, err)();
	int ret = bch2_parse_one_mount_opt(c, &opts->opts,
					   &opts->parse_later, param->key,
					   param->string,
					   &err);
	/*
	 * @err only describes a bad value; the refusals bch2_parse_one_mount_opt()
	 * returns directly (not a mount option, quota without
	 * CONFIG_BCACHEFS_QUOTA) leave it empty, which is why this used to print
	 * a bare "Error parsing option ".
	 */
	if (ret)
		errorfc(fc, "option %s: %s", param->key,
			err.pos ? err.buf : bch2_err_str(ret));

	return bch2_err_class(ret);
}

static int bch2_fs_reconfigure(struct fs_context *fc)
{
	struct super_block *sb = fc->root->d_sb;
	struct bch2_opts_parse *opts = fc->fs_private;
	struct bch_fs *c = sb->s_fs_info;
	int ret = 0;

	opt_set(opts->opts, read_only, (fc->sb_flags & SB_RDONLY) != 0);

	bch2_reconcile_wakeup(c);

	/*
	 * If we went read-only without being asked to, we hit an error and went
	 * emergency read-only, and there's no coming back from that in this
	 * mount.
	 *
	 * Test what was asked for - fc->sb_flags - and not c->opts.read_only:
	 * emergency ro stops the filesystem without rewriting the mount option,
	 * so c->opts.read_only still says read-write and the comparison below
	 * finds nothing to do, reporting success while leaving the filesystem
	 * read-only.
	 */
	if (!(fc->sb_flags & SB_RDONLY) &&
	    test_bit(BCH_FS_emergency_ro, &c->flags)) {
		errorfc(fc, "cannot go read-write: filesystem is in emergency read-only");
		return bch_err_throw(c, emergency_ro);
	}

	if (opts->opts.read_only != c->opts.read_only) {
		guard(rwsem_write)(&c->state_lock);

		if (opts->opts.read_only) {
			try(sync_filesystem(sb));

			bch2_fs_read_only(c);

			sb->s_flags |= SB_RDONLY;
		} else {
			/* __bch2_fs_read_write() gates reconcile on this: */
			c->opts.read_only = false;

			ret = bch2_fs_read_write(c);
			if (ret) {
				c->opts.read_only = true;
				/* bch_err() says which filesystem, errorfc() reaches
				 * the caller; the throw below discards the errcode. */
				bch_err(c, "error going read-write: %s", bch2_err_str(ret));
				errorfc(fc, "error going read-write: %s", bch2_err_str(ret));
				return bch_err_throw(c, EINVAL_reconfigure_read_write);
			}

			sb->s_flags &= ~SB_RDONLY;
		}

		c->opts.read_only = opts->opts.read_only;
	}

	if (opt_defined(opts->opts, errors))
		c->opts.errors = opts->opts.errors;

	return bch2_err_class(ret);
}

static const struct fs_context_operations bch2_context_ops = {
	.free        = bch2_fs_context_free,
	.parse_param = bch2_fs_parse_param,
	.get_tree    = bch2_fs_get_tree,
	.reconfigure = bch2_fs_reconfigure,
};

static int bch2_init_fs_context(struct fs_context *fc)
{
	struct bch2_opts_parse *opts = kzalloc(sizeof(*opts), GFP_KERNEL);

	if (!opts)
		return -ENOMEM;

	opts->parse_later = PRINTBUF;

	fc->ops = &bch2_context_ops;
	fc->fs_private = opts;

	return 0;
}

void bch2_fs_vfs_exit(struct bch_fs *c)
{
	bioset_exit(&c->vfs.dio_write_bioset);
	bioset_exit(&c->vfs.dio_read_bioset);
	bioset_exit(&c->vfs.writepage_bioset);
	bioset_exit(&c->vfs.nocow_flush_bioset);
	mempool_exit(&c->vfs.writepage_buf_pool);

	if (c->vfs.writeback_wq)
		destroy_workqueue(c->vfs.writeback_wq);
	if (c->vfs.inodes_by_inum_table.ht.tbl)
		rhltable_destroy(&c->vfs.inodes_by_inum_table);
	if (c->vfs.inodes_table.tbl)
		rhashtable_destroy(&c->vfs.inodes_table);
	fast_list_exit(&c->vfs.inodes);
}

int bch2_fs_vfs_init(struct bch_fs *c)
{
	fdm_init(&c->fdm_table);
	try(fast_list_init(&c->vfs.inodes));

	try(rhashtable_init(&c->vfs.inodes_table, &bch2_vfs_inodes_params));
	try(rhltable_init(&c->vfs.inodes_by_inum_table, &bch2_vfs_inodes_by_inum_params));

	if (bioset_init(&c->vfs.dio_read_bioset,
			4, offsetof(struct dio_read, rbio.bio),
			BIOSET_NEED_BVECS))
		return bch_err_throw(c, ENOMEM_dio_read_bioset_init);

	return 0;
}

int bch2_fs_vfs_init_rw(struct bch_fs *c)
{
	if (bioset_init(&c->vfs.writepage_bioset,
			4, offsetof(struct bch_writepage_io, op.wbio.bio),
			BIOSET_NEED_BVECS))
		return bch_err_throw(c, ENOMEM_writepage_bioset_init);

	if (mempool_init_kvmalloc_pool(&c->vfs.writepage_buf_pool, 4,
				       BCH_WRITEPAGE_BUF_BYTES))
		return bch_err_throw(c, ENOMEM_writepage_buf_pool_init);

	if (bioset_init(&c->vfs.dio_write_bioset,
			4, offsetof(struct dio_write, op.wbio.bio),
			BIOSET_NEED_BVECS))
		return bch_err_throw(c, ENOMEM_dio_write_bioset_init);

	if (bioset_init(&c->vfs.nocow_flush_bioset,
			1, offsetof(struct nocow_flush, bio), 0))
		return bch_err_throw(c, ENOMEM_nocow_flush_bioset_init);

	c->vfs.writeback_wq = alloc_workqueue("bcachefs_vfs_writeback",
					      WQ_MEM_RECLAIM|WQ_FREEZABLE|WQ_UNBOUND, 1);
	if (!c->vfs.writeback_wq)
		return bch_err_throw(c, ENOMEM_fs_other_alloc);

	return 0;
}

static struct file_system_type bcache_fs_type = {
	.owner			= THIS_MODULE,
	.name			= "bcachefs",
	.init_fs_context	= bch2_init_fs_context,
	.kill_sb		= bch2_kill_sb,
	.fs_flags		= FS_REQUIRES_DEV | FS_ALLOW_IDMAP | FS_LBS,
};

MODULE_ALIAS_FS("bcachefs");

void bch2_vfs_exit(void)
{
	unregister_filesystem(&bcache_fs_type);
	kmem_cache_destroy(bch2_inode_cache);
}

int __init bch2_vfs_init(void)
{
	int ret = -ENOMEM;

	bch2_inode_cache = KMEM_CACHE(bch_inode_info, SLAB_RECLAIM_ACCOUNT |
				      SLAB_ACCOUNT);
	if (!bch2_inode_cache)
		goto err;

	ret = register_filesystem(&bcache_fs_type);
	if (ret)
		goto err;

	return 0;
err:
	bch2_vfs_exit();
	return ret;
}

#endif /* NO_BCACHEFS_FS */
