// SPDX-License-Identifier: GPL-2.0
#ifndef NO_BCACHEFS_FS

#include "bcachefs.h"

#include "fs/dirent.h"
#include "fs/inode.h"
#include "fs/namei.h"
#include "fs/quota.h"

#include "snapshots/snapshot.h"
#include "snapshots/subvolume.h"

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
	 * Atomicity: take create_lock as a writer to block new page-cache
	 * dirtiers (write_iter, page_mkwrite), then flush existing dirty
	 * pages. Writeback's folio_clear_dirty_for_io path WPs every PTE
	 * pointing at a dirty folio (via folio_mkclean), so when sync
	 * returns no writable PTE remains — any subsequent mmap store
	 * traps to page_mkwrite, which then blocks on the writer.
	 */
	percpu_down_write(&c->snapshots.create_lock);
	if (arg.flags & BCH_SUBVOL_SNAPSHOT_CREATE) {
		scoped_guard(rwsem_read, &c->vfs_sb->s_umount)
			sync_inodes_sb(c->vfs_sb);
	}
	inode = __bch2_create(file_mnt_idmap(filp), to_bch_ei(dir),
			      dst_dentry, arg.mode|S_IFDIR,
			      0, snapshot_src, create_flags);
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

/* What the Rust dispatch, bch2_fs_file_ioctl(), hasn't converted yet */
long __bch2_fs_file_ioctl(struct file *file, unsigned cmd, unsigned long arg)
{
	struct bch_inode_info *inode = file_bch_inode(file);
	struct bch_fs *c = inode->v.i_sb->s_fs_info;
	long ret;

	switch (cmd) {
	case BCHFS_IOC_REINHERIT_ATTRS:
		ret = bch2_ioc_reinherit_attrs(c, file, inode,
					       (void __user *) arg);
		break;

	case FS_IOC_SETVERSION:
		ret = -ENOTTY;
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

	case BCHFS_IOC_PREAD_RAW:
		ret = bch2_ioc_pread_raw(file, inode,
				(struct bch_ioctl_pread_raw __user *) arg);
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
