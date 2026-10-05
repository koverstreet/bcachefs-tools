// SPDX-License-Identifier: GPL-2.0
#include "bcachefs.h"
#include "bcachefs_ioctl.h"

#include "btree/cache.h"
#include "btree/update.h"

#include "fs/check.h"
#include "fs/inode.h"

#include "init/error.h"
#include "init/passes.h"
#include "init/fs.h"

#include "snapshots/snapshot.h"

#include "vfs/fs.h"

#include "util/darray.h"
#include "util/thread_with_file.h"

int bch2_reconstruct_subvol(struct btree_trans *trans, u32 snapshotid, u32 subvolid, u64 inum)
{
	struct bch_fs *c = trans->c;

	if (!bch2_snapshot_is_leaf(c, snapshotid)) {
		bch_err(c, "need to reconstruct subvol, but have interior node snapshot");
		return bch_err_throw(c, fsck_repair_unimplemented);
	}

	/*
	 * Without an inum from the caller, find the root inode rather than
	 * minting one: the inode carrying bi_subvol == subvolid is the root,
	 * and when it's the subvolume key that went missing that inode is
	 * still there. Creating a second one would leave two claimants for the
	 * same subvolume and the real contents orphaned behind the new empty
	 * root.
	 *
	 * It can't be deferred to a later pass either - bch2_subvolume_validate()
	 * rejects a subvolume key with inode == 0 (subvol_inode_bad), so the
	 * key can't be written at all until we know it.
	 */
	if (!inum) {
		struct bkey_s_c k;
		int ret = 0;

		for_each_btree_key_norestart(trans, iter, BTREE_ID_inodes, POS_MIN,
					     BTREE_ITER_prefetch|BTREE_ITER_all_snapshots, k, ret) {
			if (!bkey_is_inode(k.k))
				continue;

			struct bch_inode_unpacked candidate;
			bch2_inode_unpack(c, k, &candidate);

			if (candidate.bi_subvol == subvolid) {
				inum = candidate.bi_inum;
				break;
			}
		}
		if (ret)
			return ret;

		if (!inum) {
			bch_err(c, "no root inode found for subvol %u, can't reconstruct",
				subvolid);
			return bch_err_throw(c, fsck_repair_unimplemented);
		}
	}

	bch_info(c, "reconstructing subvol %u with root inode %llu", subvolid, inum);

	struct bkey_i_subvolume *new_subvol = errptr_try(bch2_trans_kmalloc(trans, sizeof(*new_subvol)));

	bkey_subvolume_init(&new_subvol->k_i);
	new_subvol->k.p.offset	= subvolid;
	new_subvol->v.snapshot	= cpu_to_le32(snapshotid);
	new_subvol->v.inode	= cpu_to_le64(inum);
	bch2_subvolume_state_set(&new_subvol->v, SUBVOLUME_STATE_live);
	try(bch2_btree_insert_trans(trans, BTREE_ID_subvolumes, &new_subvol->k_i, 0));

	struct bkey_i_snapshot *s = bch2_bkey_get_mut_typed(trans,
			BTREE_ID_snapshots, POS(0, snapshotid),
			0, snapshot);
	int ret = PTR_ERR_OR_ZERO(s);
	bch_err_msg(c, ret, "getting snapshot %u", snapshotid);
	if (ret)
		return ret;

	u32 snapshot_tree = le32_to_cpu(s->v.tree);

	s->v.subvol = cpu_to_le32(subvolid);
	bch2_snapshot_state_set(&s->v, SNAPSHOT_STATE_live);

	struct bkey_i_snapshot_tree *st = bch2_bkey_get_mut_typed(trans,
			BTREE_ID_snapshot_trees, POS(0, snapshot_tree),
			0, snapshot_tree);
	ret = PTR_ERR_OR_ZERO(st);
	bch_err_msg(c, ret, "getting snapshot tree %u", snapshot_tree);
	if (ret)
		return ret;

	if (!st->v.master_subvol)
		st->v.master_subvol = cpu_to_le32(subvolid);
	return 0;
}

/* translate to return code of fsck commad - man(8) fsck */
int bch2_fs_fsck_errcode(struct bch_fs *c, struct printbuf *msg)
{
	int ret = 0;

	if (test_bit(BCH_FS_errors_fixed, &c->flags)) {
		prt_printf(msg, "%s: errors fixed\n", c->name);
		ret |= 1;
	}
	if (test_bit(BCH_FS_error, &c->flags)) {
		prt_printf(msg, "%s: still has errors\n", c->name);
		ret |= 4;
	}
	if (test_bit(BCH_FS_emergency_ro, &c->flags)) {
		prt_printf(msg, "%s: fatal error (went emergency read-only)\n", c->name);
		ret |= 8;
	}

	return ret;
}

#ifndef NO_BCACHEFS_CHARDEV

struct fsck_thread {
	struct thread_with_stdio thr;
	struct bch_fs		*c;
	struct bch_opts		opts;
};

static void bch2_fsck_thread_exit(struct thread_with_stdio *_thr)
{
	struct fsck_thread *thr = container_of(_thr, struct fsck_thread, thr);
	kfree(thr);
}

static int bch2_fsck_offline_thread_fn(struct thread_with_stdio *stdio)
{
	struct fsck_thread *thr = container_of(stdio, struct fsck_thread, thr);
	struct bch_fs *c = thr->c;

	errptr_try(c);

	c->recovery_task = current;

	int ret = bch2_fs_start(c);

	CLASS(printbuf, buf)();
	if (ret) {
		prt_printf(&buf, "%s: error starting filesystem: %s\n", c->name, bch2_err_str(ret));
		/*
		 * What we return is an fsck(8) exit status, not an errcode -
		 * see bch2_fs_fsck_errcode(). A filesystem we couldn't start
		 * is an operational error, same as the online path reports
		 * for recovery passes that fail outright.
		 */
		ret = 8;
	} else
		ret = bch2_fs_fsck_errcode(c, &buf);
	if (ret)
		bch2_stdio_redirect_write(&stdio->stdio, false, buf.buf, buf.pos);

	bch2_fs_exit(c);
	return ret;
}

static const struct thread_with_stdio_ops bch2_offline_fsck_ops = {
	.exit		= bch2_fsck_thread_exit,
	.fn		= bch2_fsck_offline_thread_fn,
};

static int parse_mount_opts_user(char __user *optstr_user, struct bch_opts *opts)
{
	char *optstr __free(kfree) = errptr_try(strndup_user(optstr_user, 1 << 16));

	return bch2_parse_mount_opts(NULL, opts, NULL, optstr, false);
}

long bch2_ioctl_fsck_offline(struct bch_ioctl_fsck_offline __user *user_arg)
{
	struct bch_ioctl_fsck_offline arg;

	try(copy_from_user_errcode(&arg, user_arg, sizeof(arg)));

	if (arg.flags)
		return -BCH_ERR_EINVAL_fsck_offline_bad_flags;

	if (!capable(CAP_SYS_ADMIN))
		return -EPERM;

	struct bch_opts opts = bch2_opts_empty();
	if (arg.opts)
		try(parse_mount_opts_user((char __user *)(unsigned long) arg.opts, &opts));

	CLASS(darray_const_str, devs)();
	for (size_t i = 0; i < arg.nr_devs; i++) {
		u64 dev_u64;
		try(copy_from_user_errcode(&dev_u64, &user_arg->devs[i], sizeof(u64)));

		char *dev_str =
			errptr_try(strndup_user((char __user *)(unsigned long) dev_u64, PATH_MAX));

		int ret = darray_push(&devs, dev_str);
		if (ret) {
			kfree(dev_str);
			return ret;
		}
	}

	struct fsck_thread *thr = kzalloc(sizeof(*thr), GFP_KERNEL);
	if (!thr)
		return -ENOMEM;

	thr->opts = opts;

	opt_set(thr->opts, stdio, (u64)(unsigned long)&thr->thr.stdio);
	opt_set(thr->opts, read_only, 1);
	opt_set(thr->opts, ratelimit_errors, 0);

	/* We need request_key() to be called before we punt to kthread: */
	opt_set(thr->opts, nostart, true);

	bch2_thread_with_stdio_init(&thr->thr, &bch2_offline_fsck_ops);

	thr->c = bch2_fs_open(&devs, &thr->opts, NULL);

	if (!IS_ERR(thr->c) &&
	    thr->c->opts.errors == BCH_ON_ERROR_panic)
		thr->c->opts.errors = BCH_ON_ERROR_ro;

	int ret = __bch2_run_thread_with_stdio(&thr->thr);
	if (ret < 0) {
		if (thr)
			bch2_fsck_thread_exit(&thr->thr);
		pr_err("ret %s", bch2_err_str(ret));
	}
	return ret;
}

static int bch2_fsck_online_thread_fn(struct thread_with_stdio *stdio)
{
	struct fsck_thread *thr = container_of(stdio, struct fsck_thread, thr);
	struct bch_fs *c = thr->c;
	CLASS(printbuf, buf)();
	int ret = -EAGAIN;

	u64 online = bch2_recovery_passes_match(PASS_ONLINE);
	u64 passes = bch2_recovery_passes_match(PASS_FSCK) & online;

	if (opt_defined(thr->opts, recovery_passes)) {
		passes = thr->opts.recovery_passes;

		if ((passes & online) != passes) {
			prt_printf(&buf, "Cannot run passes ");
			prt_bitflags(&buf, bch2_recovery_passes, passes & ~online);
			prt_printf(&buf, " online\n");
			bch2_stdio_redirect_write(&stdio->stdio, false, buf.buf, buf.pos);
			return bch_err_throw(c, EINVAL_fsck_online_bad_passes);
		}
	}

	if (mutex_trylock(&c->recovery.run_lock)) {
		c->stdio_filter = current;
		c->stdio = &thr->thr.stdio;

		/*
		 * XXX: can we figure out a way to do this without mucking with c->opts?
		 */
		unsigned old_fix_errors = c->opts.fix_errors;
		if (opt_defined(thr->opts, fix_errors))
			c->opts.fix_errors = thr->opts.fix_errors;
		else
			c->opts.fix_errors = FSCK_FIX_ask;

		c->opts.fsck = true;
		set_bit(BCH_FS_in_fsck, &c->flags);

		ret = bch2_run_recovery_passes(c, passes, true) ?:
			bch2_fs_fsck_errcode(c, &buf);

		clear_bit(BCH_FS_in_fsck, &c->flags);

		c->stdio = NULL;
		c->stdio_filter = NULL;
		c->opts.fix_errors = old_fix_errors;

		mutex_unlock(&c->recovery.run_lock);
	}
	bch2_ro_ref_put(c);

	if (ret < 0) {
		prt_printf(&buf, "%s: error running recovery passes: %s\n", c->name, bch2_err_str(ret));
		ret = 8;
	}

	if (buf.pos)
		bch2_stdio_redirect_write(&stdio->stdio, false, buf.buf, buf.pos);
	return ret;
}

static const struct thread_with_stdio_ops bch2_online_fsck_ops = {
	.exit		= bch2_fsck_thread_exit,
	.fn		= bch2_fsck_online_thread_fn,
};

long bch2_ioctl_fsck_online(struct bch_fs *c, struct bch_ioctl_fsck_online arg)
{
	if (arg.flags)
		return bch_err_throw(c, EINVAL_fsck_online_bad_flags);

	if (!capable(CAP_SYS_ADMIN))
		return bch_err_throw(c, EPERM_non_admin);

	struct bch_opts opts = bch2_opts_empty();
	if (arg.opts)
		try(parse_mount_opts_user((char __user *)(unsigned long) arg.opts, &opts));

	if (!bch2_ro_ref_tryget(c))
		return -EROFS;

	struct fsck_thread *thr = kzalloc(sizeof(*thr), GFP_KERNEL);
	if (!thr) {
		bch2_ro_ref_put(c);
		return -ENOMEM;
	}

	thr->c = c;
	thr->opts = opts;

	int ret = bch2_run_thread_with_stdio(&thr->thr, &bch2_online_fsck_ops);
	if (ret < 0) {
		bch_err_fn(c, ret);
		bch2_fsck_thread_exit(&thr->thr);
		bch2_ro_ref_put(c);
	}
	return ret;
}

#endif /* NO_BCACHEFS_CHARDEV */
