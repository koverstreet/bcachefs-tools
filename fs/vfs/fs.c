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
#include "vfs/fs_type_gen.h"
#include "vfs/buffered.h"
#include "vfs/direct.h"
#include "vfs/pagecache.h"

#include <linux/aio.h>
#include <linux/compat.h>
#include <linux/fileattr.h>
#include <linux/module.h>
#include <linux/pagemap.h>
#include <linux/posix_acl.h>
#include <linux/random.h>
#include <linux/security.h>
#include <linux/seq_file.h>
#include <linux/siphash.h>
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

/* The filesystem type is vfs/fs_type.rs's: */
MODULE_ALIAS_FS("bcachefs");

void bch2_vfs_exit(void)
{
	bch2_fs_type_unregister();
	kmem_cache_destroy(bch2_inode_cache);
}

int __init bch2_vfs_init(void)
{
	int ret = -ENOMEM;

	bch2_inode_cache = KMEM_CACHE(bch_inode_info, SLAB_RECLAIM_ACCOUNT |
				      SLAB_ACCOUNT);
	if (!bch2_inode_cache)
		goto err;

	ret = bch2_fs_type_register();
	if (ret)
		goto err;

	return 0;
err:
	bch2_vfs_exit();
	return ret;
}

#endif /* NO_BCACHEFS_FS */
