// SPDX-License-Identifier: GPL-2.0
#ifndef NO_BCACHEFS_FS

#include "bcachefs.h"

#include "fs/dirent.h"
#include "fs/inode.h"
#include "fs/namei.h"
#include "init/damage.h"
#include "fs/quota.h"

#include "snapshots/snapshot.h"
#include "snapshots/subvolume.h"

#include "alloc/accounting.h"
#include "btree/write_buffer.h"
#include "data/reconcile/trigger.h"
#include "data/reflink_format.h"

#include "init/chardev.h"
#include "init/fs.h"

#include "vfs/direct.h"
#include "vfs/fs.h"
#include "vfs/ioctl.h"

#include <linux/compat.h>
#include <linux/fsnotify.h>
#include <linux/mount.h>
#include <linux/namei.h>
#include <linux/version.h>
#include <linux/security.h>
#include <linux/writeback.h>

#define FS_IOC_GOINGDOWN	     _IOR('X', 125, __u32)
#define FSOP_GOING_FLAGS_DEFAULT	0x0	/* going down */
#define FSOP_GOING_FLAGS_LOGFLUSH	0x1	/* flush log but not data */
#define FSOP_GOING_FLAGS_NOLOGFLUSH	0x2	/* don't flush log nor data */

/*
 * The VFS gained start_removing_user_path_at()/end_removing_path() in 6.18.
 * The create-side helpers map straight onto user_path_create()/
 * done_path_create(), which already bundle the mount write ref — but the
 * removal side needs a real wrapper: user_path_locked_at() only does the
 * locked lookup, leaving the write ref to the caller.
 */
#if LINUX_VERSION_CODE < KERNEL_VERSION(6,18,0)
#define start_creating_user_path	user_path_create
#define end_creating_path		done_path_create
#define end_removing_path		done_path_create

static inline struct dentry *
start_removing_user_path_at(int dfd, const char __user *name, struct path *path)
{
	struct dentry *victim = user_path_locked_at(dfd, name, path);
	if (IS_ERR(victim))
		return victim;

	struct inode *dir = path->dentry->d_inode;

	/*
	 * sb_writers nests outside i_rwsem: drop the parent lock that
	 * user_path_locked_at() took, acquire the write ref, relock, and
	 * revalidate the victim across the unlocked window. (>=6.18's
	 * start_removing_user_path_at() takes the write ref before the
	 * locked lookup and so has no such window.)
	 */
	inode_unlock(dir);
	int ret = mnt_want_write(path->mnt);
	inode_lock(dir);

	if (!ret && (d_unhashed(victim) || victim->d_parent != path->dentry)) {
		mnt_drop_write(path->mnt);
		ret = -ENOENT;
	}

	if (ret) {
		inode_unlock(dir);
		dput(victim);
		path_put(path);
		return ERR_PTR(ret);
	}

	return victim;
}
#elif LINUX_VERSION_CODE >= KERNEL_VERSION(7,2,0)
/*
 * 7.2 removed start_removing_user_path_at() (commit e6666aef1105), leaving no
 * dfd + user-pointer removal-lookup helper. Rebuild it from the pieces that
 * remain: user_path_at() honours the dfd and resolves the victim, then
 * start_removing_dentry() locks the parent and revalidates that victim under
 * the lock (d_parent/d_unhashed). The mount write ref is taken here, since
 * start_removing_dentry() does only the lock -- matching the contract
 * end_removing_path() expects (parent locked, write ref held).
 */
static inline struct dentry *
start_removing_user_path_at(int dfd, const char __user *name, struct path *path)
{
	struct path victim;
	int ret = user_path_at(dfd, name, 0, &victim);
	if (ret)
		return ERR_PTR(ret);

	ret = mnt_want_write(victim.mnt);
	if (ret) {
		path_put(&victim);
		return ERR_PTR(ret);
	}

	struct dentry *parent = dget_parent(victim.dentry);
	struct dentry *child = start_removing_dentry(parent, victim.dentry);
	if (IS_ERR(child)) {
		dput(parent);
		mnt_drop_write(victim.mnt);
		path_put(&victim);
		return child;
	}

	/*
	 * Hand back the parent path (locked, write ref held) plus the victim.
	 * Transfer victim.mnt into *path (keeping its ref and the write ref);
	 * release the resolved victim dentry -- child is a fresh ref.
	 */
	path->mnt	= victim.mnt;
	path->dentry	= parent;
	dput(victim.dentry);
	return child;
}
#endif

static int bch2_reinherit_attrs_fn(struct btree_trans *trans,
				   struct bch_inode_info *inode,
				   struct bch_inode_unpacked *bi,
				   void *p)
{
	struct bch_inode_info *dir = p;

	return !bch2_reinherit_attrs(bi, &dir->ei_inode);
}

static int bch2_ioc_reinherit_attrs(struct bch_fs *c,
				    struct file *file,
				    struct bch_inode_info *src,
				    const char __user *name)
{
	struct bch_inode_info *dst;
	struct inode *vinode = NULL;
	char *kname = NULL;
	struct qstr qstr;
	int ret = 0;
	subvol_inum inum;

	struct bch_hash_info hash;
	try(bch2_hash_info_init(c, &src->ei_inode, &hash));

	kname = kmalloc(BCH_NAME_MAX, GFP_KERNEL);
	if (!kname)
		return -ENOMEM;

	ret = strncpy_from_user(kname, name, BCH_NAME_MAX);
	if (unlikely(ret < 0))
		goto err1;

	qstr.len	= ret;
	qstr.name	= kname;

	ret = bch2_dirent_lookup(c, inode_inum(src), &hash, &qstr, &inum);
	if (ret)
		goto err1;

	vinode = bch2_vfs_inode_get(c, inum, __func__);
	ret = PTR_ERR_OR_ZERO(vinode);
	if (ret)
		goto err1;

	dst = to_bch_ei(vinode);

	ret = mnt_want_write_file(file);
	if (ret)
		goto err2;

	bch2_lock_inodes(INODE_UPDATE_LOCK, src, dst);

	if (inode_attr_changing(src, dst, Inode_opt_project)) {
		ret = bch2_fs_quota_transfer(c, dst,
					     src->ei_qid,
					     1 << QTYP_PRJ,
					     KEY_TYPE_QUOTA_PREALLOC);
		if (ret)
			goto err3;
	}

	ret = bch2_write_inode(c, dst, bch2_reinherit_attrs_fn, src, 0);
err3:
	bch2_unlock_inodes(INODE_UPDATE_LOCK, src, dst);

	/* return true if we did work */
	if (ret >= 0)
		ret = !ret;

	mnt_drop_write_file(file);
err2:
	iput(vinode);
err1:
	kfree(kname);

	return ret;
}

static int bch2_ioc_getversion(struct bch_inode_info *inode, u32 __user *arg)
{
	return put_user(inode->v.i_generation, arg);
}

static int bch2_ioc_getlabel(struct bch_fs *c, char __user *user_label)
{
	int ret;
	size_t len;
	char label[BCH_SB_LABEL_SIZE];

	BUILD_BUG_ON(BCH_SB_LABEL_SIZE >= FSLABEL_MAX);

	scoped_guard(mutex_noio, &c->sb_lock)
		memcpy(label, c->disk_sb.sb->label, BCH_SB_LABEL_SIZE);

	len = strnlen(label, BCH_SB_LABEL_SIZE);
	if (len == BCH_SB_LABEL_SIZE) {
		bch_warn(c,
			"label is too long, return the first %zu bytes",
			--len);
	}

	ret = copy_to_user(user_label, label, len);

	return ret ? -EFAULT : 0;
}

static int bch2_ioc_setlabel(struct bch_fs *c,
			     struct file *file,
			     struct bch_inode_info *inode,
			     const char __user *user_label)
{
	if (!capable(CAP_SYS_ADMIN))
		return bch_err_throw(c, EPERM_non_admin);

	char label[BCH_SB_LABEL_SIZE];
	if (copy_from_user(label, user_label, sizeof(label)))
		return -EFAULT;

	if (strnlen(label, BCH_SB_LABEL_SIZE) == BCH_SB_LABEL_SIZE) {
		bch_err(c,
			"unable to set label with more than %d bytes",
			BCH_SB_LABEL_SIZE - 1);
		return bch_err_throw(c, EINVAL_setlabel_too_long);
	}

	try(mnt_want_write_file(file));

	int ret;
	scoped_guard(mutex_noio, &c->sb_lock) {
		strscpy(c->disk_sb.sb->label, label, BCH_SB_LABEL_SIZE);
		ret = bch2_write_super(c);
	}

	mnt_drop_write_file(file);
	return ret;
}

static int bch2_ioc_goingdown(struct bch_fs *c, u32 __user *arg)
{
	if (!capable(CAP_SYS_ADMIN))
		return bch_err_throw(c, EPERM_non_admin);

	u32 flags;
	try(get_user(flags, arg));

	CLASS(bch_log_msg, msg)(c);
	msg.m.suppress = true; /* cleared by ERO */

	prt_printf(&msg.m, "shutdown by ioctl type %u", flags);

	switch (flags) {
	case FSOP_GOING_FLAGS_DEFAULT:
		try(bdev_freeze(c->vfs_sb->s_bdev));

		bch2_journal_flush(&c->journal);
		bch2_fs_emergency_read_only(c, &msg.m);

		bdev_thaw(c->vfs_sb->s_bdev);
		return 0;
	case FSOP_GOING_FLAGS_LOGFLUSH:
		bch2_journal_flush(&c->journal);
		fallthrough;
	case FSOP_GOING_FLAGS_NOLOGFLUSH:
		bch2_fs_emergency_read_only(c, &msg.m);
		return 0;
	default:
		return bch_err_throw(c, EINVAL_goingdown_bad_flags);
	}
}

static long __bch2_ioctl_subvolume_create(struct bch_fs *c, struct file *filp,
					  struct bch_ioctl_subvolume_v2 arg,
					  struct printbuf *err)
{
	struct inode *dir;
	struct bch_inode_info *inode;
	struct user_namespace *s_user_ns;
	struct dentry *dst_dentry;
	struct path src_path, dst_path;
	int how = LOOKUP_FOLLOW;
	int error;
	subvol_inum snapshot_src = { 0 };
	unsigned lookup_flags = 0;
	unsigned create_flags = BCH_CREATE_SUBVOL;

	if (arg.flags & ~(BCH_SUBVOL_SNAPSHOT_CREATE|
			  BCH_SUBVOL_SNAPSHOT_RO)) {
		prt_str(err, "invalid flasg");
		return bch_err_throw(c, EINVAL_subvol_create_bad_flags);
	}

	if (!(arg.flags & BCH_SUBVOL_SNAPSHOT_CREATE) &&
	    (arg.src_ptr ||
	     (arg.flags & BCH_SUBVOL_SNAPSHOT_RO))) {
		prt_str(err, "invalid flasg");
		return bch_err_throw(c, EINVAL_subvol_create_flags_mismatch);
	}

	if (arg.flags & BCH_SUBVOL_SNAPSHOT_CREATE)
		create_flags |= BCH_CREATE_SNAPSHOT;

	if (arg.flags & BCH_SUBVOL_SNAPSHOT_RO)
		create_flags |= BCH_CREATE_SNAPSHOT_RO;

	if (arg.src_ptr) {
		error = user_path_at(arg.dirfd,
				(const char __user *)(unsigned long)arg.src_ptr,
				how, &src_path);
		if (error)
			goto err1;

		if (src_path.dentry->d_sb->s_fs_info != c) {
			path_put(&src_path);
			prt_str(err, "src_path not on dst filesystem");
			error = -EXDEV;
			goto err1;
		}

		snapshot_src = inode_inum(to_bch_ei(src_path.dentry->d_inode));
	}

	dst_dentry = start_creating_user_path(arg.dirfd,
			(const char __user *)(unsigned long)arg.dst_ptr,
			&dst_path, lookup_flags);
	error = PTR_ERR_OR_ZERO(dst_dentry);
	if (error)
		goto err2;

	if (dst_dentry->d_sb->s_fs_info != c) {
		prt_str(err, "dst_path not on dst filesystem");
		error = -EXDEV;
		goto err3;
	}

	if (dst_dentry->d_inode) {
		error = bch_err_throw(c, EEXIST_subvolume_create);
		goto err3;
	}

	dir = dst_path.dentry->d_inode;
	if (IS_DEADDIR(dir)) {
		error = bch_err_throw(c, ENOENT_directory_dead);
		goto err3;
	}

	s_user_ns = dir->i_sb->s_user_ns;
	if (!kuid_has_mapping(s_user_ns, current_fsuid()) ||
	    !kgid_has_mapping(s_user_ns, current_fsgid())) {
		prt_str(err, "current uid/gid not mapped into fs namespace");
		error = -EOVERFLOW;
		goto err3;
	}

	error = inode_permission(file_mnt_idmap(filp),
				 dir, MAY_WRITE | MAY_EXEC);
	if (error)
		goto err3;

	if (!IS_POSIXACL(dir))
		arg.mode &= ~current_umask();

	error = security_path_mkdir(&dst_path, dst_dentry, arg.mode);
	if (error)
		goto err3;

	if ((arg.flags & BCH_SUBVOL_SNAPSHOT_CREATE) &&
	    !arg.src_ptr)
		snapshot_src.subvol = inode_inum(to_bch_ei(dir)).subvol;

	/*
	 * Atomicity: write-lock create_lock to block new syscall-level
	 * dirtiers (write_iter), then pagefault_lock to block mmap dirtiers
	 * (page_mkwrite), then flush existing dirty pages. Two locks because
	 * the two dirtier classes sit on opposite sides of mmap_lock — see
	 * the comment above bch_fs_snapshots. Writeback's
	 * folio_clear_dirty_for_io path WPs every PTE pointing at a dirty
	 * folio (via folio_mkclean), so when sync returns no writable PTE
	 * remains — any subsequent mmap store traps to page_mkwrite, which
	 * then blocks on pagefault_lock.
	 */
	percpu_down_write(&c->snapshots.create_lock);
	percpu_down_write(&c->snapshots.pagefault_lock);
	if (arg.flags & BCH_SUBVOL_SNAPSHOT_CREATE) {
		scoped_guard(rwsem_read, &c->vfs_sb->s_umount)
			sync_inodes_sb(c->vfs_sb);
	}
	inode = __bch2_create(file_mnt_idmap(filp), to_bch_ei(dir),
			      dst_dentry, arg.mode|S_IFDIR,
			      0, snapshot_src, create_flags);
	percpu_up_write(&c->snapshots.pagefault_lock);
	percpu_up_write(&c->snapshots.create_lock);
	error = PTR_ERR_OR_ZERO(inode);
	if (error)
		goto err3;

	bch2_dentry_set_casefold_ops(dst_dentry, &inode->v);
	d_instantiate(dst_dentry, &inode->v);
	fsnotify_mkdir(dir, dst_dentry);
err3:
	end_creating_path(&dst_path, dst_dentry);
err2:
	if (arg.src_ptr)
		path_put(&src_path);
err1:
	return error;
}

static long bch2_ioctl_subvolume_create(struct bch_fs *c, struct file *filp,
					struct bch_ioctl_subvolume arg)
{
	struct bch_ioctl_subvolume_v2 arg_v2 = {
		.flags		= arg.flags,
		.dirfd		= arg.dirfd,
		.mode		= arg.mode,
		.dst_ptr	= arg.dst_ptr,
		.src_ptr	= arg.src_ptr,
	};

	CLASS(printbuf, err)();
	long ret = __bch2_ioctl_subvolume_create(c, filp, arg_v2, &err);
	if (ret)
		bch_err_msg(c, ret, "%s", err.buf);
	return ret;
}

static long bch2_ioctl_subvolume_create_v2(struct bch_fs *c, struct file *filp,
					   struct bch_ioctl_subvolume_v2 arg)
{
	CLASS(printbuf, err)();
	long ret = __bch2_ioctl_subvolume_create(c, filp, arg, &err);
	return bch2_copy_ioctl_err_msg(&arg.err, &err, ret);
}

static long __bch2_ioctl_subvolume_destroy(struct bch_fs *c, struct file *filp,
					   struct bch_ioctl_subvolume_v2 arg,
					   struct printbuf *err)
{
	int ret = 0;

	if (arg.flags)
		return bch_err_throw(c, EINVAL_subvol_destroy_bad_flags);

	const char __user *name = (void __user *)(unsigned long)arg.dst_ptr;
	struct path path;
	struct dentry *victim = errptr_try(start_removing_user_path_at(arg.dirfd, name, &path));

	struct inode *dir = d_inode(path.dentry);
	if (victim->d_sb->s_fs_info != c) {
		ret = -EXDEV;
		goto err;
	}

	/*
	 * start_removing_user_path_at() returns with the parent locked and
	 * the mount write ref held; end_removing_path() drops both.
	 */
	ret =   inode_permission(file_mnt_idmap(filp), d_inode(victim), MAY_WRITE) ?:
		__bch2_unlink(dir, victim, true);
	if (!ret) {
		fsnotify_rmdir(dir, victim);
		d_invalidate(victim);
	}
err:
	end_removing_path(&path, victim);
	return ret;
}

static long bch2_ioctl_subvolume_destroy(struct bch_fs *c, struct file *filp,
					 struct bch_ioctl_subvolume arg)
{
	struct bch_ioctl_subvolume_v2 arg_v2 = {
		.flags		= arg.flags,
		.dirfd		= arg.dirfd,
		.mode		= arg.mode,
		.dst_ptr	= arg.dst_ptr,
		.src_ptr	= arg.src_ptr,
	};

	CLASS(printbuf, err)();
	long ret = __bch2_ioctl_subvolume_destroy(c, filp, arg_v2, &err);
	if (ret && err.buf)
		bch_err_msg(c, ret, "%s", err.buf);
	return ret;
}

static long bch2_ioctl_subvolume_destroy_v2(struct bch_fs *c, struct file *filp,
					    struct bch_ioctl_subvolume_v2 arg)
{
	CLASS(printbuf, err)();
	long ret = __bch2_ioctl_subvolume_destroy(c, filp, arg, &err);
	return bch2_copy_ioctl_err_msg(&arg.err, &err, ret);
}

/*
 * Check if the current user can traverse from a child subvolume root
 * up to the parent subvolume, checking MAY_EXEC on each intermediate
 * directory using the full VFS permission stack (including POSIX ACLs
 * and LSM hooks).
 *
 * Returns 0 if accessible, 1 to skip (permission denied or path doesn't
 * connect to parent), or negative on error.
 */
static inline void bch2_iput(struct bch_inode_info *inode) { iput(&inode->v); }
DEFINE_DARRAY_NAMED_FREE_ITEM(darray_inode, struct bch_inode_info *, bch2_iput);

static int bch2_check_path_accessible(struct btree_trans *trans,
				      struct mnt_idmap *idmap,
				      struct bch_subvolume *child,
				      u32 child_subvol, u32 parent_subvol)
{
	struct bch_inode_info *inode = bch2_vfs_inode_get_trans(trans,
			(subvol_inum) { child_subvol, le64_to_cpu(child->inode) },
			__func__);
	if (IS_ERR(inode))
		return PTR_ERR(inode);

	u32 parent_sv = inode->ei_inode.bi_parent_subvol;
	u64 dir_inum = inode->ei_inode.bi_dir;
	iput(&inode->v);

	if (!parent_sv)
		return -EIO;

	CLASS(darray_inode, check_inodes)();

	while (dir_inum) {
		inode = bch2_vfs_inode_get_trans(trans,
				(subvol_inum) { parent_sv, dir_inum }, __func__);
		if (IS_ERR(inode))
			return PTR_ERR(inode);

		int ret = darray_push(&check_inodes, inode);
		if (ret) {
			iput(&inode->v);
			return ret;
		}

		if (inode->ei_inode.bi_subvol == parent_subvol)
			goto check_perms;

		dir_inum = inode->ei_inode.bi_dir;
	}

	return 1;
check_perms:
	/*
	 * Unlock the transaction before calling inode_permission(),
	 * which may trigger bch2_get_acl() needing its own transaction.
	 */
	bch2_trans_unlock(trans);

	darray_for_each(check_inodes, i) {
		int ret = inode_permission(idmap, &(*i)->v, MAY_EXEC);
		if (ret)
			return 1;
	}

	return bch2_trans_relock(trans);
}

static int bch2_subvol_readdir_emit(struct btree_trans *trans,
				    struct mnt_idmap *idmap,
				    u32 parent, u32 child_subvol,
				    char __user *buf, u32 buf_size,
				    u32 *used, u32 *pos)
{
	struct bch_subvolume child;
	try(bch2_subvolume_get(trans, child_subvol, true, &child));

	int ret = bch2_check_path_accessible(trans, idmap, &child, child_subvol, parent);
	if (ret) {
		if (ret > 0) {
			*pos = child_subvol + 1;
			ret = 0;
		}
		return ret;
	}

	CLASS(printbuf, path)();
	ret = bch2_inum_to_path_in_subvol(trans,
		(subvol_inum) { child_subvol, le64_to_cpu(child.inode) },
		parent, INUM_TO_PATH_FAIL_ON_ERR, &path);
	if (ret) {
		if (!bch2_err_matches(ret, BCH_ERR_transaction_restart)) {
			*pos = child_subvol + 1;
			ret = 0;
		}
		return ret;
	}

	/*
	 * Strip leading '/' — bch2_inum_to_path_in_subvol() stopped at @parent,
	 * so paths are relative to the parent subvolume's root, not to whatever
	 * directory the caller's fd names.
	 */
	char *p = path.buf;
	u32 len = path.pos;
	while (len && *p == '/') { p++; len--; }

	u32 path_bytes = len + 1;
	u32 reclen = ALIGN(offsetof(struct bch_ioctl_subvol_dirent, path) +
			   path_bytes, 8);

	if (*used + reclen > buf_size)
		return 1;

	struct timespec64 otime = bch2_time_to_timespec(trans->c,
						le64_to_cpu(child.otime.lo));

	struct bch_ioctl_subvol_dirent ent = {
		.reclen		= reclen,
		.subvolid	= child_subvol,
		.flags		= le32_to_cpu(child.flags),
		.snapshot_parent = le32_to_cpu(child.creation_parent),
		.otime_sec	= otime.tv_sec,
		.otime_nsec	= otime.tv_nsec,
	};

	try(copy_to_user_errcode(buf + *used, &ent, sizeof(ent)));
	try(copy_to_user_errcode(buf + *used + sizeof(ent), p, path_bytes));

	/* Zero-fill alignment padding between NUL terminator and next entry */
	u32 written = sizeof(ent) + path_bytes;
	if (written < reclen &&
	    clear_user(buf + *used + written, reclen - written))
		return -EFAULT;

	*used += reclen;
	*pos = child_subvol + 1;
	return 0;
}

static long bch2_ioctl_subvolume_list(struct bch_fs *c, struct file *filp,
				      struct bch_ioctl_subvol_readdir __user *user_arg)
{
	struct bch_ioctl_subvol_readdir arg;
	try(copy_from_user_errcode(&arg, user_arg, sizeof(arg)));

	if (arg.pad)
		return bch_err_throw(c, EINVAL_subvol_readdir_pad);

	u32 parent = inode_inum(file_bch_inode(filp)).subvol;
	struct mnt_idmap *idmap = file_mnt_idmap(filp);

	char __user *buf = (char __user *)(unsigned long)arg.buf;
	u32 used = 0;
	u32 pos = arg.pos;

	CLASS(btree_trans, trans)(c);

	int ret = for_each_btree_key(trans, iter,
			BTREE_ID_subvolume_children,
			POS(parent, arg.pos),
			BTREE_ITER_prefetch, k, ({
		if (k.k->p.inode != parent)
			break;

		int ret2 = bch2_subvol_readdir_emit(trans, idmap,
						    parent, k.k->p.offset,
						    buf, arg.buf_size,
						    &used, &pos);
		if (ret2 > 0)
			break;
		ret2;
	}));

	if (ret)
		return ret;

	try(put_user(pos, &user_arg->pos));
	try(put_user(used, &user_arg->used));

	return 0;
}

static long bch2_ioctl_subvolume_to_path(struct bch_fs *c, struct file *filp,
					 struct bch_ioctl_subvol_to_path __user *user_arg)
{
	struct bch_ioctl_subvol_to_path arg;
	try(copy_from_user_errcode(&arg, user_arg, sizeof(arg)));

	if (!arg.buf_size)
		return bch_err_throw(c, EINVAL_subvol_to_path_no_buf);

	CLASS(btree_trans, trans)(c);
	CLASS(printbuf, path)();

	struct bch_subvolume subvol;
	int ret = lockrestart_do(trans, ({
		printbuf_reset(&path);
		bch2_subvolume_get(trans, arg.subvolid, false, &subvol) ?:
		bch2_inum_to_path(trans,
			(subvol_inum) { arg.subvolid, le64_to_cpu(subvol.inode) },
			&path);
	}));
	if (ret)
		return ret;

	/* Strip leading '/' — return path relative to mountpoint */
	char *p = path.buf;
	u32 len = path.pos;
	while (len && *p == '/') { p++; len--; }

	u32 path_bytes = len + 1; /* include NUL */
	if (path_bytes > arg.buf_size)
		return -ERANGE;

	char __user *ubuf = (char __user *)(unsigned long)arg.buf;
	try(copy_to_user_errcode(ubuf, p, path_bytes));

	return 0;
}

static long bch2_ioc_get_damage(struct bch_fs *c, struct file *filp,
				struct bch_ioctl_get_damage __user *user_arg)
{
	struct bch_ioctl_get_damage arg;
	try(copy_from_user_errcode(&arg, user_arg, sizeof(arg)));

	if (arg.pad)
		return -EINVAL;

	subvol_inum inum = inode_inum(file_bch_inode(filp));

	CLASS(btree_trans, trans)(c);
	bch_sb_errors_cpu errors = {};

	int ret = lockrestart_do(trans, ({
		errors.nr = 0;
		u32 snapshot;
		bch2_subvolume_get_snapshot(trans, inum.subvol, &snapshot) ?:
		bch2_damage_accumulate(trans, inum.inum, snapshot, &errors);
	}));

	for (u32 i = 0; !ret && i < min_t(u32, errors.nr, arg.nr_entries); i++) {
		bch_sb_field_error_entry_v2 e = {};
		SET_BCH_SB_ERROR_ENTRY_V2_ID(&e,	errors.data[i].id);
		SET_BCH_SB_ERROR_ENTRY_V2_NR(&e,	errors.data[i].nr);
		SET_BCH_SB_ERROR_ENTRY_V2_FIRST(&e,	errors.data[i].first_error_time);
		SET_BCH_SB_ERROR_ENTRY_V2_LAST(&e,	errors.data[i].last_error_time);

		ret = copy_to_user_errcode(&user_arg->entries[i], &e, sizeof(e));
	}
	if (!ret)
		ret = put_user((u32) errors.nr, &user_arg->nr_entries);

	darray_exit(&errors);
	return ret;
}

static long bch2_ioc_clear_damage(struct bch_fs *c, struct file *filp)
{
	struct bch_inode_info *inode = file_bch_inode(filp);

	/* forensic metadata: erasing it takes ownership, like chattr */
	if (!inode_owner_or_capable(file_mnt_idmap(filp), &inode->v))
		return -EPERM;

	try(mnt_want_write_file(filp));

	CLASS(btree_trans, trans)(c);
	int ret = commit_do(trans, NULL, NULL, BCH_TRANS_COMMIT_no_enospc,
			    bch2_damage_clear(trans, inode_inum(inode)));

	mnt_drop_write_file(filp);
	return ret;
}

static int bch2_readdir_flags_emit(const struct qstr *name, u64 inum,
				   u8 d_type,
				   char __user *buf, u32 buf_size, u32 *used)
{
	u32 name_offset = offsetof(struct bch_ioctl_readdir_entry, name);
	u32 name_bytes = name->len + 1;
	u32 reclen = ALIGN(name_offset + name_bytes, 8);

	if (*used + reclen > buf_size)
		return 1;

	struct bch_ioctl_readdir_entry ent = {
		.inum		= inum,
		.d_type		= d_type,
		.name_len	= name_bytes,
	};

	try(copy_to_user_errcode(buf + *used, &ent, name_offset));
	try(copy_to_user_errcode(buf + *used + name_offset, name->name, name->len));

	u32 written = name_offset + name->len;
	if (clear_user(buf + *used + written, reclen - written))
		return -EFAULT;

	*used += reclen;
	return 0;
}

#define BCH_READDIR_FLAGS_ALL		(BCH_READDIR_recursive|	\
					 BCH_READDIR_damaged|\
					 BCH_READDIR_subvolumes_only)

/*
 * Does this entry pass the requested filter? > 0 keep, 0 skip, < 0
 * error. New filters are new cases; the iteration doesn't change.
 */
static int bch2_readdir_filter(struct btree_trans *trans, u32 flags,
			       subvol_inum dir, u32 view,
			       struct bkey_s_c_dirent d, subvol_inum target)
{
	switch (flags & ~BCH_READDIR_recursive) {
	case 0:
		return 1;
	case BCH_READDIR_damaged: {
		/* A subvolume dirent's target has its own lineage: */
		u32 snap = view;
		if (target.subvol != dir.subvol)
			try(bch2_subvolume_get_snapshot(trans, target.subvol,
							&snap));

		return bch2_inode_has_damage(trans, target.inum, snap);
	}
	case BCH_READDIR_subvolumes_only:
		return d.v->d_type == DT_SUBVOL;
	default:
		return -EINVAL;
	}
}

/*
 * Shared machinery for the recursive modes: each iterates its filter's
 * candidate set (the damage btree, the subvolumes btree) rather than
 * scanning dirents, then qualifies every candidate the same way - is it
 * under this directory, and what is its path relative to it?
 */
struct readdir_recursive {
	subvol_inum		dir;
	struct mnt_idmap	*idmap;
	struct printbuf		*dir_path;
	struct printbuf		*path;
	char __user		*buf;
	u32			buf_size;
	u32			used;
};

enum readdir_recursive_res {
	READDIR_EMITTED,
	READDIR_BUF_FULL,
	READDIR_SKIP,
};

/*
 * A recursive listing tunnels past the directories a dirent walk would
 * have to descend through, so enforce the permissions that walk would
 * have hit: MAY_READ on the candidate's parent (the candidate's name is
 * that directory's content), MAY_EXEC on every directory above it, up
 * to but excluding the fd's directory, which open() already checked.
 * Full VFS permission stack (POSIX ACLs, LSM hooks), same shape as
 * bch2_check_path_accessible(). Returns 0 if accessible, 1 to skip,
 * negative on error; racing renames and unreachable parents skip.
 */
static int readdir_recursive_path_accessible(struct btree_trans *trans,
					     struct mnt_idmap *idmap,
					     subvol_inum n, subvol_inum dir)
{
	struct bch_inode_info *inode = bch2_vfs_inode_get_trans(trans, n, __func__);
	if (IS_ERR(inode))
		return bch2_err_matches(PTR_ERR(inode), ENOENT) ? 1 : PTR_ERR(inode);

	subvol_inum parent = {
		.subvol	= inode->ei_inode.bi_parent_subvol ?: n.subvol,
		.inum	= inode->ei_inode.bi_dir,
	};
	iput(&inode->v);

	CLASS(darray_inode, check_inodes)();
	unsigned depth = 0;

	while (parent.subvol != dir.subvol || parent.inum != dir.inum) {
		/*
		 * bch2_inum_is_descendant() just vetted this chain cycle-free;
		 * the depth cap only guards against a rename racing in between:
		 */
		if (!parent.inum || ++depth > 4096)
			return 1;

		inode = bch2_vfs_inode_get_trans(trans, parent, __func__);
		if (IS_ERR(inode))
			return bch2_err_matches(PTR_ERR(inode), ENOENT)
				? 1 : PTR_ERR(inode);

		int ret = darray_push(&check_inodes, inode);
		if (ret) {
			iput(&inode->v);
			return ret;
		}

		parent = (subvol_inum) {
			.subvol	= inode->ei_inode.bi_parent_subvol ?: parent.subvol,
			.inum	= inode->ei_inode.bi_dir,
		};
	}

	/*
	 * Unlock the transaction before calling inode_permission(), which
	 * may trigger bch2_get_acl() needing its own transaction:
	 */
	bch2_trans_unlock(trans);

	darray_for_each(check_inodes, i) {
		unsigned mask = i == check_inodes.data ? MAY_READ : MAY_EXEC;
		if (inode_permission(idmap, &(*i)->v, mask))
			return 1;
	}

	return bch2_trans_relock(trans);
}

/*
 * Qualify @n and emit it: check it's under the directory, build its path,
 * relativize against the directory's own path, check the caller may see
 * it, copy out. Disagreement between the descendant walk and path
 * resolution (racing rename) just skips the entry - as does the
 * directory itself, whose relative path would be empty.
 */
static int readdir_recursive_emit(struct btree_trans *trans,
				  struct readdir_recursive *r,
				  subvol_inum n, u8 d_type)
{
	int ret = bch2_inum_is_descendant(trans, n, r->dir);
	if (ret <= 0)
		return ret ?: READDIR_SKIP;

	printbuf_reset(r->path);
	try(bch2_inum_to_path(trans, n, r->path));

	u32 plen = r->dir_path->pos;
	if (!(r->path->pos > plen &&
	      !memcmp(r->path->buf, r->dir_path->buf, plen) &&
	      (plen == 1 || r->path->buf[plen] == '/')))
		return READDIR_SKIP;

	u32 rel = plen == 1 ? 1 : plen + 1;
	struct qstr name = {
		.name	= r->path->buf + rel,
		.len	= r->path->pos - rel,
	};

	/* name_len is u16 in the entry format; don't truncate, skip: */
	if (name.len + 1 > U16_MAX)
		return READDIR_SKIP;

	ret = readdir_recursive_path_accessible(trans, r->idmap, n, r->dir);
	if (ret)
		return ret < 0 ? ret : READDIR_SKIP;

	ret = bch2_readdir_flags_emit(&name, n.inum, d_type,
				      r->buf, r->buf_size, &r->used);
	return ret < 0 ? ret : ret ? READDIR_BUF_FULL : READDIR_EMITTED;
}

/*
 * Recursive damaged doesn't scan dirents: it iterates the damage
 * btree - cost proportional to recorded damage, not tree size - and
 * checks each candidate inode for being under this directory. Damage
 * keys are sorted by inum, so consecutive snapshot versions of one
 * inode dedup by remembering the last inum emitted.
 */
static long bch2_ioc_readdir_recursive_damaged(struct bch_fs *c,
			struct file *filp,
			struct bch_ioctl_readdir_flags *arg,
			struct bch_ioctl_readdir_flags __user *user_arg)
{
	struct bpos pos = SPOS(0, arg->pos[0], arg->pos[1]);

	CLASS(btree_trans, trans)(c);
	CLASS(printbuf, dir_path)();
	CLASS(printbuf, path)();
	struct readdir_recursive r = {
		.dir		= inode_inum(file_bch_inode(filp)),
		.idmap		= file_mnt_idmap(filp),
		.dir_path	= &dir_path,
		.path		= &path,
		.buf		= (char __user *)(unsigned long)arg->buf,
		.buf_size	= arg->buf_size,
	};

	u32 view;
	try(lockrestart_do(trans, ({
		printbuf_reset(&dir_path);
		bch2_subvolume_get_snapshot(trans, r.dir.subvol, &view) ?:
		bch2_inum_to_path(trans, r.dir, &dir_path);
	})));

	u64 last_handled = 0;
	int ret = for_each_btree_key(trans, iter, BTREE_ID_damage, pos,
				     BTREE_ITER_prefetch|BTREE_ITER_all_snapshots, k, ({
		int ret2 = 0;
		u64 inum = k.k->p.offset;

		if (inum != last_handled &&
		    bch2_snapshot_is_ancestor(trans, view, k.k->p.snapshot)) {
			if (k.k->type != KEY_TYPE_damage) {
				/*
				 * A whiteout: damage was cleared in this view,
				 * which also hides what ancestor versions
				 * recorded - skip the inum entirely:
				 */
				last_handled = inum;
			} else {
				ret2 = readdir_recursive_emit(trans, &r,
						(subvol_inum) { r.dir.subvol, inum },
						DT_UNKNOWN);
				if (ret2 == READDIR_BUF_FULL)
					break;
				if (ret2 == READDIR_EMITTED)
					last_handled = inum;
				if (ret2 > 0)
					ret2 = 0;
			}
		}

		/*
		 * Once an inum is handled its other snapshot versions are
		 * irrelevant, and the resume cursor must reflect that: if the
		 * buffer filled between two versions of one inode, restarting
		 * at the next key would emit the inum a second time.
		 */
		if (!ret2)
			pos = inum == last_handled
				? SPOS(0, inum + 1, 0)
				: bpos_successor(k.k->p);
		ret2;
	}));

	if (ret)
		return ret;

	try(put_user(pos.offset, &user_arg->pos[0]));
	try(put_user((u64) pos.snapshot, &user_arg->pos[1]));
	try(put_user(r.used, &user_arg->used));

	return 0;
}

/*
 * Recursive subvolumes_only iterates the subvolumes btree - cost
 * proportional to the number of subvolumes - qualifying each live
 * subvolume's root by ancestry through the shared machinery. One key
 * per subvolume, so no dedup, and the cursor is just the next
 * subvolume id.
 */
static long bch2_ioc_readdir_recursive_subvols(struct bch_fs *c,
			struct file *filp,
			struct bch_ioctl_readdir_flags *arg,
			struct bch_ioctl_readdir_flags __user *user_arg)
{
	u64 pos = arg->pos[0];

	CLASS(btree_trans, trans)(c);
	CLASS(printbuf, dir_path)();
	CLASS(printbuf, path)();
	struct readdir_recursive r = {
		.dir		= inode_inum(file_bch_inode(filp)),
		.idmap		= file_mnt_idmap(filp),
		.dir_path	= &dir_path,
		.path		= &path,
		.buf		= (char __user *)(unsigned long)arg->buf,
		.buf_size	= arg->buf_size,
	};

	try(lockrestart_do(trans, ({
		printbuf_reset(&dir_path);
		bch2_inum_to_path(trans, r.dir, &dir_path);
	})));

	int ret = for_each_btree_key(trans, iter, BTREE_ID_subvolumes,
				     POS(0, pos), BTREE_ITER_prefetch, k, ({
		int ret2 = 0;

		if (k.k->type == KEY_TYPE_subvolume) {
			struct bkey_s_c_subvolume s = bkey_s_c_to_subvolume(k);

			/*
			 * Live subvolumes only: unlinked and deleted ones have
			 * no dirent - they're in nobody's namespace, and path
			 * resolution would fail:
			 */
			if (bch2_subvolume_state_compat(s.v) == SUBVOLUME_STATE_live) {
				ret2 = readdir_recursive_emit(trans, &r,
						(subvol_inum) { k.k->p.offset,
								le64_to_cpu(s.v->inode) },
						DT_SUBVOL);
				if (ret2 == READDIR_BUF_FULL)
					break;
				if (ret2 > 0)
					ret2 = 0;
			}
		}
		if (!ret2)
			pos = k.k->p.offset + 1;
		ret2;
	}));

	if (ret)
		return ret;

	try(put_user(pos, &user_arg->pos[0]));
	try(put_user((u64) 0, &user_arg->pos[1]));
	try(put_user(r.used, &user_arg->used));

	return 0;
}

static long bch2_ioc_readdir_flags(struct bch_fs *c, struct file *filp,
				   struct bch_ioctl_readdir_flags __user *user_arg)
{
	struct bch_ioctl_readdir_flags arg;
	try(copy_from_user_errcode(&arg, user_arg, sizeof(arg)));

	if (arg.pad || arg.used ||
	    (arg.flags & ~BCH_READDIR_FLAGS_ALL))
		return -EINVAL;

	struct bch_inode_info *dir_inode = file_bch_inode(filp);
	if (!S_ISDIR(dir_inode->v.i_mode))
		return -ENOTDIR;

	if (arg.flags & BCH_READDIR_recursive)
		switch (arg.flags & ~BCH_READDIR_recursive) {
		case BCH_READDIR_damaged:
			return bch2_ioc_readdir_recursive_damaged(c, filp,
							&arg, user_arg);
		case BCH_READDIR_subvolumes_only:
			return bch2_ioc_readdir_recursive_subvols(c, filp,
							&arg, user_arg);
		default:
			/* plain recursive: an honest tree walk, not yet */
			return -EOPNOTSUPP;
		}

	subvol_inum dir = inode_inum(dir_inode);
	char __user *buf = (char __user *)(unsigned long)arg.buf;
	u32 used = 0;
	u64 pos = arg.pos[0];

	CLASS(btree_trans, trans)(c);

	u32 view;
	int ret = lockrestart_do(trans,
		bch2_subvolume_get_snapshot(trans, dir.subvol, &view));

	ret = ret ?: for_each_btree_key_in_subvolume_max_in_trans(trans, iter,
			BTREE_ID_dirents,
			POS(dir.inum, pos), POS(dir.inum, U64_MAX),
			dir.subvol, 0, k, ({
		if (k.k->type != KEY_TYPE_dirent)
			continue;

		struct bkey_s_c_dirent d = bkey_s_c_to_dirent(k);
		subvol_inum target;

		int ret2 = bch2_dirent_read_target(trans, dir, d, &target);
		if (ret2 > 0)
			continue;

		ret2 = ret2 ?: bch2_readdir_filter(trans, arg.flags, dir,
						   view, d, target);
		if (ret2 > 0) {
			struct qstr name = bch2_dirent_get_name(d);
			ret2 = bch2_readdir_flags_emit(&name, target.inum,
						       d.v->d_type,
						       buf, arg.buf_size, &used);
			if (ret2 > 0)
				break;	/* buffer full - resume at pos */
		}
		if (!ret2)
			pos = k.k->p.offset + 1;
		ret2;
	}));

	if (ret)
		return ret;

	try(put_user(pos, &user_arg->pos[0]));
	try(put_user(used, &user_arg->used));

	return 0;
}

static int bch2_ioctl_snapshot_tree_resolve(struct btree_trans *trans,
					    struct file *filp, u32 arg_tree_id,
					    u32 *tree_id, struct bch_snapshot_tree *st)
{
	*tree_id = arg_tree_id;

	if (!*tree_id) {
		u32 subvolid = inode_inum(file_bch_inode(filp)).subvol;

		struct bch_subvolume subvol;
		try(bch2_subvolume_get(trans, subvolid, false, &subvol));

		*tree_id = bch2_snapshot_tree(trans->c, le32_to_cpu(subvol.snapshot));
		if (!*tree_id)
			return -ENOENT;
	}

	return bch2_snapshot_tree_lookup(trans, *tree_id, st);
}

/*
 * Shared by BCH_IOCTL_SNAPSHOT_TREE and _v2: a v1 node is a byte-prefix of a
 * v2 node, so both are served by writing @node_size bytes of a v2 node per
 * entry. v1 passes its frozen sizeof; v2 passes what the caller asked for,
 * clamped to what we have.
 */
static long __bch2_ioctl_snapshot_tree(struct bch_fs *c, struct file *filp,
				       u32 tree_id_arg, u32 size, u32 node_size,
				       void __user *user_nodes,
				       u32 __user *user_master_subvol,
				       u32 __user *user_root_snapshot,
				       u32 __user *user_nr,
				       u32 __user *user_total)
{
	/* Querying a specific tree by ID requires CAP_SYS_ADMIN */
	if (tree_id_arg && !capable(CAP_SYS_ADMIN))
		return bch_err_throw(c, EPERM_non_admin);

	u32 tree_id = tree_id_arg;
	struct bch_snapshot_tree st;
	{
		CLASS(btree_trans, trans)(c);

		int ret = lockrestart_do(trans,
			bch2_ioctl_snapshot_tree_resolve(trans, filp, tree_id_arg, &tree_id, &st));
		if (ret)
			return ret;
	}

	u32 nr = 0;
	u32 total = 0;

	CLASS(btree_trans, trans)(c);

	/* Flush write buffer so accounting keys are visible in the btree */
	try(bch2_btree_write_buffer_flush_sync(trans));

	int ret = for_each_btree_key(trans, iter,
			BTREE_ID_snapshots, POS_MIN,
			BTREE_ITER_prefetch, k, ({
		if (k.k->type != KEY_TYPE_snapshot)
			continue;

		struct bkey_s_c_snapshot snap = bkey_s_c_to_snapshot(k);
		if (le32_to_cpu(snap.v->tree) != tree_id)
			continue;

		struct bch_snapshot s;
		bkey_val_copy_pad(&s, snap);
		if (bch2_snapshot_state_compat(&s) == SNAPSHOT_STATE_deleted)
			continue;

		/*
		 * Sum [nr_keys, key_bytes, external_sectors] over the snapshot
		 * btrees (sectors only lands in the extents entry, but the key
		 * counters are per-btree):
		 */
		u64 nr_keys = 0, key_bytes = 0, sectors = 0;
		int _ret = 0;
		for (unsigned btree = 0; btree < BTREE_ID_NR && !_ret; btree++) {
			if (!btree_type_has_snapshots(btree))
				continue;

			u64 v[3] = {};
			_ret = bch2_fs_accounting_read_key2(trans, v, snapshot,
					.id = k.k->p.offset, .btree = btree);
			nr_keys		+= v[0];
			key_bytes	+= v[1];
			sectors		+= v[2];
		}

		if (!_ret) {
			total++;

			if (nr < size) {
				struct bch_ioctl_snapshot_node_v2 node = {
					.id		= k.k->p.offset,
					.parent		= le32_to_cpu(snap.v->parent),
					.children	= {
						le32_to_cpu(snap.v->children[0]),
						le32_to_cpu(snap.v->children[1]),
					},
					.subvol		= le32_to_cpu(snap.v->subvol),
					.flags		= le32_to_cpu(snap.v->flags),
					.sectors	= sectors,
					.nr_keys	= nr_keys,
					.key_bytes	= key_bytes,
				};

				_ret = copy_to_user_errcode(user_nodes + (size_t) nr * node_size,
							    &node, node_size);
				if (!_ret)
					nr++;
			}
		}
		_ret;
	}));

	if (ret)
		return ret;

	try(put_user(le32_to_cpu(st.master_subvol), user_master_subvol));
	try(put_user(le32_to_cpu(st.root_snapshot), user_root_snapshot));
	try(put_user(nr, user_nr));
	try(put_user(total, user_total));

	if (size && size < total)
		return -ERANGE;

	return 0;
}

static long bch2_ioctl_snapshot_tree(struct bch_fs *c, struct file *filp,
				     struct bch_ioctl_snapshot_tree_query __user *user_arg)
{
	struct bch_ioctl_snapshot_tree_query arg;
	try(copy_from_user_errcode(&arg, user_arg, sizeof(arg)));

	if (arg.pad)
		return bch_err_throw(c, EINVAL_snapshot_tree_query_pad);

	return __bch2_ioctl_snapshot_tree(c, filp, arg.tree_id, arg.nr,
					  sizeof(struct bch_ioctl_snapshot_node),
					  &user_arg->nodes,
					  &user_arg->master_subvol,
					  &user_arg->root_snapshot,
					  &user_arg->nr,
					  &user_arg->total);
}

static long bch2_ioctl_snapshot_tree_v2(struct bch_fs *c, struct file *filp,
					struct bch_ioctl_snapshot_tree_query_v2 __user *user_arg)
{
	struct bch_ioctl_snapshot_tree_query_v2 arg;
	try(copy_from_user_errcode(&arg, user_arg, sizeof(arg)));

	/*
	 * A caller that doesn't know its own node size can't be given one
	 * safely - we'd have to guess its stride:
	 */
	if (!arg.node_size)
		return bch_err_throw(c, EINVAL_snapshot_tree_query_pad);

	u32 node_size = min_t(u32, arg.node_size,
			      sizeof(struct bch_ioctl_snapshot_node_v2));

	/* Tell the caller what we have, so it knows which fields are set: */
	try(put_user((u32) sizeof(struct bch_ioctl_snapshot_node_v2),
		     &user_arg->node_size));

	return __bch2_ioctl_snapshot_tree(c, filp, arg.tree_id, arg.nr,
					  node_size,
					  &user_arg->nodes,
					  &user_arg->master_subvol,
					  &user_arg->root_snapshot,
					  &user_arg->nr,
					  &user_arg->total);
}

static int bch2_propagate_opts_to_reflink_v(struct btree_trans *trans,
					    struct bch_inode_opts *opts,
					    struct bkey_s_c_reflink_p p)
{
	u64 idx = REFLINK_P_IDX(p.v) - le32_to_cpu(p.v->front_pad);
	u64 end = REFLINK_P_IDX(p.v) + p.k->size + le32_to_cpu(p.v->back_pad);
	u32 restart_count = trans->restart_count;

	int ret = for_each_btree_key_commit(trans, iter, BTREE_ID_reflink,
				POS(0, idx),
				BTREE_ITER_intent|BTREE_ITER_not_extents, k,
				NULL, NULL, BCH_TRANS_COMMIT_no_enospc, ({
		if (bpos_ge(bkey_start_pos(k.k), POS(0, end)))
			break;

		bch2_update_reconcile_opts(trans, NULL, opts, &iter, 0, k,
					   SET_NEEDS_RECONCILE_opt_change);
	}));

	/* suppress trans_was_restarted() check */
	trans->restart_count = restart_count;
	return ret;
}

static long bch2_ioc_set_reflink_p_may_update_opts(struct bch_fs *c,
						   struct file *file,
						   struct bch_inode_info *inode)
{
	if (!capable(CAP_SYS_ADMIN))
		return bch_err_throw(c, EPERM_non_admin);

	try(bch2_request_incompat_feature(c, bcachefs_metadata_version_reflink_p_may_update_opts));

	subvol_inum inum = inode_inum(inode);

	CLASS(btree_trans, trans)(c);

	return for_each_btree_key_in_subvolume_max(trans, iter,
			BTREE_ID_extents,
			POS(inum.inum, 0),
			POS(inum.inum, U64_MAX),
			inum.subvol,
			BTREE_ITER_intent, k, ({
		int ret = 0;
		if (k.k->type == KEY_TYPE_reflink_p &&
		    !REFLINK_P_MAY_UPDATE_OPTIONS(bkey_s_c_to_reflink_p(k).v)) {
			struct bkey_i_reflink_p *p =
				bch2_bkey_make_mut_typed(trans, &iter, &k, 0, reflink_p);
			ret = PTR_ERR_OR_ZERO(p);
			if (!ret) {
				SET_REFLINK_P_MAY_UPDATE_OPTIONS(&p->v, true);
				ret = bch2_trans_commit(trans, NULL, NULL,
							BCH_TRANS_COMMIT_no_enospc);
			}
			if (!ret) {
				struct bch_inode_opts opts;
				ret = bch2_bkey_get_io_opts(trans, NULL, k, &opts) ?:
				      bch2_propagate_opts_to_reflink_v(trans, &opts,
								      bkey_s_c_to_reflink_p(k));
			}
		}
		ret;
	}));
}

static long bch2_ioc_propagate_reflink_p_opts(struct bch_fs *c,
				       struct file *file,
				       struct bch_inode_info *inode)
{
	if (!inode_owner_or_capable(file_mnt_idmap(file), &inode->v) &&
	    !capable(CAP_SYS_ADMIN))
		return bch_err_throw(c, EPERM_non_admin_or_owner);

	subvol_inum inum = inode_inum(inode);

	CLASS(btree_trans, trans)(c);

	return for_each_btree_key_in_subvolume_max(trans, iter,
			BTREE_ID_extents,
			POS(inum.inum, 0),
			POS(inum.inum, U64_MAX),
			inum.subvol,
			0, k, ({
		int ret = 0;
		if (k.k->type == KEY_TYPE_reflink_p) {
			if (REFLINK_P_MAY_UPDATE_OPTIONS(bkey_s_c_to_reflink_p(k).v)) {
				struct bch_inode_opts opts;
				ret = bch2_bkey_get_io_opts(trans, NULL, k, &opts) ?:
				      bch2_propagate_opts_to_reflink_v(trans, &opts,
								      bkey_s_c_to_reflink_p(k));
			} else if (!capable(CAP_SYS_ADMIN)) {
				ret = bch_err_throw(c, reflink_p_may_update_options_unset);
			}
		}
		ret;
	}));
}

static long bch2_ioc_pread_raw(struct file *file,
			       struct bch_inode_info *inode,
			       struct bch_ioctl_pread_raw __user *uarg)
{
	struct bch_ioctl_pread_raw arg;
	struct bch_fs *c = file->f_inode->i_sb->s_fs_info;

	if (copy_from_user(&arg, uarg, sizeof(arg)))
		return -EFAULT;
	if (arg.flags & ~BCH_PREAD_RAW_no_poison_check)
		return -EINVAL;
	if (arg.err.pad)
		return -EINVAL;
	if (!arg.len)
		return 0;
	if (!(file->f_flags & O_DIRECT))
		return -EINVAL;
	if (!inode_owner_or_capable(file_mnt_idmap(file), &inode->v))
		return bch_err_throw(c, EPERM_non_admin_or_owner);

	loff_t pos = arg.offset;
	int ret = rw_verify_area(READ, file, &pos, arg.len);
	if (ret)
		return ret;

	struct iov_iter iter;
	import_ubuf(ITER_DEST, (void __user *)(unsigned long)arg.buf, arg.len, &iter);

	struct kiocb kiocb;
	init_sync_kiocb(&kiocb, file);
	kiocb.ki_pos = arg.offset;

	enum bch_read_flags read_flags = 0;
	if (arg.flags & BCH_PREAD_RAW_no_poison_check)
		read_flags |= BCH_READ_no_poison_check;

	struct bch_read_err_report err_report;
	mutex_init(&err_report.lock);
	err_report.errors = 0;
	err_report.msg = (struct printbuf) PRINTBUF;

	ret = bch2_direct_IO_read(&kiocb, &iter, read_flags, &err_report);

	if (copy_to_user(&uarg->errors, &err_report.errors, sizeof(err_report.errors)))
		ret = -EFAULT;

	int err = bch2_copy_ioctl_err_msg(&arg.err, &err_report.msg, ret < 0 ? ret : 0);
	if (err && !ret)
		ret = err;

	printbuf_exit(&err_report.msg);
	return ret;
}

static int bch2_unpoison_extent(struct btree_trans *trans, struct btree_iter *iter,
			       struct bkey_s_c k)
{
	u64 flags = bch2_bkey_extent_flags(k);
	if (!(flags & BIT_ULL(BCH_EXTENT_FLAG_poisoned)))
		return 0;

	struct bkey_i *new = errptr_try(bch2_trans_kmalloc(trans,
					bkey_bytes(k.k) + sizeof(struct bch_extent_flags)));

	bkey_reassemble(new, k);
	try(bch2_bkey_extent_flags_set(trans->c, new,
				       flags & ~BIT_ULL(BCH_EXTENT_FLAG_poisoned)));
	try(bch2_trans_update(trans, iter, new, 0));
	return 0;
}

static int bch2_unpoison_reflink(struct btree_trans *trans,
				 struct bkey_s_c_reflink_p p)
{
	u64 idx = REFLINK_P_IDX(p.v);
	u64 end = idx + p.k->size;

	struct bkey_s_c k;
	int ret;
	for_each_btree_key_norestart(trans, iter, BTREE_ID_reflink,
			POS(0, idx), BTREE_ITER_intent, k, ret) {
		if (bpos_ge(bkey_start_pos(k.k), POS(0, end)))
			break;
		try(bch2_unpoison_extent(trans, &iter, k));
	}
	return ret;
}

static long bch2_ioc_unpoison(struct bch_fs *c, struct file *file,
			      struct bch_inode_info *inode,
			      struct bch_ioctl_unpoison __user *uarg)
{
	struct bch_ioctl_unpoison arg;

	if (copy_from_user(&arg, uarg, sizeof(arg)))
		return -EFAULT;
	if (arg.flags || arg.pad)
		return -EINVAL;
	if (!arg.len)
		return 0;

	if (!inode_owner_or_capable(file_mnt_idmap(file), &inode->v))
		return bch_err_throw(c, EPERM_non_admin_or_owner);

	subvol_inum inum = inode_inum(inode);
	struct bpos start = POS(inum.inum, arg.offset >> 9);
	struct bpos end   = POS(inum.inum, (arg.offset + arg.len) >> 9);

	CLASS(btree_trans, trans)(c);

	return for_each_btree_key_in_subvolume_max(trans, iter, BTREE_ID_extents,
			start, end, inum.subvol, BTREE_ITER_intent, k, ({
		(k.k->type == KEY_TYPE_reflink_p
			? bch2_unpoison_reflink(trans, bkey_s_c_to_reflink_p(k))
			: bch2_unpoison_extent(trans, &iter, k)) ?:
		bch2_trans_commit(trans, NULL, NULL, 0);
	}));
}

long bch2_fs_file_ioctl(struct file *file, unsigned cmd, unsigned long arg)
{
	struct bch_inode_info *inode = file_bch_inode(file);
	struct bch_fs *c = inode->v.i_sb->s_fs_info;
	long ret;

	switch (cmd) {
	case BCHFS_IOC_REINHERIT_ATTRS:
		ret = bch2_ioc_reinherit_attrs(c, file, inode,
					       (void __user *) arg);
		break;

	case BCHFS_IOC_GET_DAMAGE:
		ret = bch2_ioc_get_damage(c, file, (void __user *) arg);
		break;

	case BCHFS_IOC_READDIR_FLAGS:
		ret = bch2_ioc_readdir_flags(c, file, (void __user *) arg);
		break;

	case BCHFS_IOC_CLEAR_DAMAGE:
		ret = bch2_ioc_clear_damage(c, file);
		break;

	case BCHFS_IOC_SET_REFLINK_P_MAY_UPDATE_OPTS:
		ret = bch2_ioc_set_reflink_p_may_update_opts(c, file, inode);
		break;

	case BCHFS_IOC_PROPAGATE_REFLINK_P_OPTS:
		ret = bch2_ioc_propagate_reflink_p_opts(c, file, inode);
		break;

	case FS_IOC_GETVERSION:
		ret = bch2_ioc_getversion(inode, (u32 __user *) arg);
		break;

	case FS_IOC_SETVERSION:
		ret = -ENOTTY;
		break;

	case FS_IOC_GETFSLABEL:
		ret = bch2_ioc_getlabel(c, (void __user *) arg);
		break;

	case FS_IOC_SETFSLABEL:
		ret = bch2_ioc_setlabel(c, file, inode, (const void __user *) arg);
		break;

	case FS_IOC_GOINGDOWN:
		ret = bch2_ioc_goingdown(c, (u32 __user *) arg);
		break;

	case BCH_IOCTL_SUBVOLUME_CREATE: {
		struct bch_ioctl_subvolume i;

		ret = copy_from_user(&i, (void __user *) arg, sizeof(i))
			? -EFAULT
			: bch2_ioctl_subvolume_create(c, file, i);
		break;
	}

	case BCH_IOCTL_SUBVOLUME_CREATE_v2: {
		struct bch_ioctl_subvolume_v2 i;

		ret = copy_from_user(&i, (void __user *) arg, sizeof(i))
			? -EFAULT
			: bch2_ioctl_subvolume_create_v2(c, file, i);
		break;
	}

	case BCH_IOCTL_SUBVOLUME_DESTROY: {
		struct bch_ioctl_subvolume i;

		ret = copy_from_user(&i, (void __user *) arg, sizeof(i))
			? -EFAULT
			: bch2_ioctl_subvolume_destroy(c, file, i);
		break;
	}

	case BCH_IOCTL_SUBVOLUME_DESTROY_v2: {
		struct bch_ioctl_subvolume_v2 i;

		ret = copy_from_user(&i, (void __user *) arg, sizeof(i))
			? -EFAULT
			: bch2_ioctl_subvolume_destroy_v2(c, file, i);
		break;
	}

	case BCH_IOCTL_SUBVOLUME_LIST:
		ret = bch2_ioctl_subvolume_list(c, file,
				(struct bch_ioctl_subvol_readdir __user *) arg);
		break;

	case BCH_IOCTL_SUBVOLUME_TO_PATH:
		ret = bch2_ioctl_subvolume_to_path(c, file,
				(struct bch_ioctl_subvol_to_path __user *) arg);
		break;

	case BCH_IOCTL_SNAPSHOT_TREE:
		ret = bch2_ioctl_snapshot_tree(c, file,
				(struct bch_ioctl_snapshot_tree_query __user *) arg);
		break;

	case BCH_IOCTL_SNAPSHOT_TREE_v2:
		ret = bch2_ioctl_snapshot_tree_v2(c, file,
				(struct bch_ioctl_snapshot_tree_query_v2 __user *) arg);
		break;

	case BCHFS_IOC_PREAD_RAW:
		ret = bch2_ioc_pread_raw(file, inode,
				(struct bch_ioctl_pread_raw __user *) arg);
		break;

	case BCHFS_IOC_UNPOISON:
		ret = bch2_ioc_unpoison(c, file, inode,
				(struct bch_ioctl_unpoison __user *) arg);
		break;

	default:
		ret = bch2_fs_ioctl(c, cmd, (void __user *) arg);
		break;
	}

	return bch2_err_class(ret);
}

#ifdef CONFIG_COMPAT
long bch2_compat_fs_ioctl(struct file *file, unsigned cmd, unsigned long arg)
{
	/* These are just misnamed, they actually get/put from/to user an int */
	switch (cmd) {
	case FS_IOC32_GETFLAGS:
		cmd = FS_IOC_GETFLAGS;
		break;
	case FS_IOC32_SETFLAGS:
		cmd = FS_IOC_SETFLAGS;
		break;
	case FS_IOC32_GETVERSION:
		cmd = FS_IOC_GETVERSION;
		break;
	case FS_IOC_GETFSLABEL:
	case FS_IOC_SETFSLABEL:
		break;
	default:
		return -ENOIOCTLCMD;
	}
	return bch2_fs_file_ioctl(file, cmd, (unsigned long) compat_ptr(arg));
}
#endif

#endif /* NO_BCACHEFS_FS */
