// SPDX-License-Identifier: GPL-2.0
//
// C shims for the Rust FUSE mount command. Wraps inline kernel functions
// and complex operations (transactions, closures, bio I/O) that can't be
// called directly from Rust via bindgen.

#ifdef BCACHEFS_FUSE

#include <errno.h>
#include <string.h>

#include "libbcachefs.h"
#include "fs/bcachefs.h"
#include "fs/fs/dirent.h"
#include "fs/fs/namei.h"
#include "fs/fs/inode.h"
#include "fs/alloc/accounting.h"
#include "fs/alloc/buckets.h"
#include "fs/alloc/foreground.h"
#include "fs/data/read.h"
#include "fs/data/write.h"
#include "fs/btree/iter.h"
#include "fs/init/fs.h"
#include "fs/fs/xattr.h"
#include "fs/snapshots/subvolume.h"

#include <linux/dcache.h>
#include <linux/xattr.h>

#include "fuse_shims.h"

/* ---- thread initialization ---- */

/*
 * fuser worker threads don't run sched_init() (it's a constructor for
 * the main thread only). Any libbcachefs code that touches 'current'
 * will NULL-deref without this.
 */
void rust_fuse_ensure_current(void)
{
	if (current)
		return;

	struct task_struct *p = calloc(1, sizeof(*p));
	p->state = TASK_RUNNING;
	atomic_set(&p->usage, 1);
	init_completion(&p->exited);
	current = p;
}

void rust_fuse_rcu_register(void)
{
	rcu_register_thread();
	bch_percpu_thread_init();
}

void rust_fuse_rcu_unregister(void)
{
	rcu_unregister_thread();
}


/* ---- readdir ---- */

struct rust_readdir_ctx {
	struct dir_context	ctx;
	void			*opaque;
	rust_fuse_filldir_fn	filldir;
};

static int rust_fuse_readdir_actor(struct dir_context *_ctx,
				   const char *name, int namelen,
				   loff_t pos, u64 ino, unsigned type)
{
	struct rust_readdir_ctx *rctx =
		container_of(_ctx, struct rust_readdir_ctx, ctx);

	/*
	 * A fuse dirent's offset field is the cookie to resume the listing
	 * *after* that entry; pos is the entry's own dirent offset
	 * (bch2_dir_emit() sets ctx->pos before emitting). If we reject on a
	 * full reply buffer, ctx->pos never advances past this entry and the
	 * next readdir re-reads it.
	 */
	return rctx->filldir(rctx->opaque, name, (unsigned)namelen,
			     ino, type, (u64)(pos + 1));
}

int rust_fuse_readdir(struct bch_fs *c, subvol_inum dir,
		      u64 pos, void *ctx, rust_fuse_filldir_fn filldir)
{
	struct bch_inode_unpacked bi;
	int ret = bch2_inode_find_by_inum(c, dir, &bi);
	if (ret)
		return ret;

	struct bch_hash_info dir_hash;
	ret = bch2_hash_info_init(c, &bi, &dir_hash);
	if (ret)
		return ret;

	struct rust_readdir_ctx rctx = {
		.ctx.actor	= rust_fuse_readdir_actor,
		.ctx.pos	= pos,
		.opaque		= ctx,
		.filldir	= filldir,
	};

	return bch2_readdir(c, dir, &dir_hash, &rctx.ctx);
}

/*
 * xattrs by full name ("user.foo") on an inode given by number.
 *
 * xattr.c's get and list take VFS objects (bch_inode_info, dentry), and for the
 * kernel the VFS does the prefix to type mapping. These do the inode lookup and
 * that mapping, then what xattr.c does. A stopgap: once xattr.c is Rust they
 * fold into it.
 *
 * Only user., trusted. and security. are served. system.posix_acl_* is stored
 * in bcachefs's own ACL format and bcachefs.* names inode options rather than
 * xattrs - both need conversions this doesn't do. Matching on
 * bch2_xattr_handlers[] instead of this table would be wrong for the latter:
 * the bcachefs.* handlers have no .flags, so "bcachefs.foo" would read as the
 * user xattr "foo".
 */
static const char * const rust_fuse_xattr_prefixes[] = {
	[KEY_TYPE_XATTR_INDEX_USER]	= XATTR_USER_PREFIX,
	[KEY_TYPE_XATTR_INDEX_TRUSTED]	= XATTR_TRUSTED_PREFIX,
	[KEY_TYPE_XATTR_INDEX_SECURITY]	= XATTR_SECURITY_PREFIX,
};

/* Strips the prefix from *name; returns the xattr type, or -EOPNOTSUPP. */
static int rust_fuse_xattr_type(const char **name)
{
	for (unsigned i = 0; i < ARRAY_SIZE(rust_fuse_xattr_prefixes); i++) {
		const char *prefix = rust_fuse_xattr_prefixes[i];

		if (prefix && !strncmp(*name, prefix, strlen(prefix))) {
			*name += strlen(prefix);
			return i;
		}
	}
	return -EOPNOTSUPP;
}

static int __rust_fuse_xattr_get(struct btree_trans *trans, subvol_inum inum,
			    int type, const char *name, void *buf, size_t size)
{
	struct bch_inode_unpacked inode_u;
	try(bch2_inode_find_by_inum_trans(trans, inum, &inode_u));

	return bch2_xattr_get_trans(trans, &inode_u, inum, type, name, buf, size);
}

/* The value's length; with @buf NULL, just the length. */
int rust_fuse_xattr_get(struct bch_fs *c, subvol_inum inum, const char *name,
		   void *buf, size_t size)
{
	int type = rust_fuse_xattr_type(&name);
	if (type < 0)
		return type;

	CLASS(btree_trans, trans)(c);
	int ret = lockrestart_do(trans, __rust_fuse_xattr_get(trans, inum, type, name, buf, size));

	/* as bch2_xattr_get_handler(): a missing xattr is ENODATA */
	return bch2_err_matches(ret, ENOENT) ? -ENODATA : ret;
}

static int rust_fuse_xattr_emit(char *buf, size_t size, size_t *used,
			   const char *prefix, const char *name, unsigned name_len)
{
	size_t len = strlen(prefix) + name_len + 1;

	if (buf) {
		if (*used + len > size)
			return -ERANGE;
		memcpy(buf + *used, prefix, strlen(prefix));
		memcpy(buf + *used + strlen(prefix), name, name_len);
		buf[*used + len - 1] = '\0';
	}
	*used += len;
	return 0;
}

/*
 * NUL separated full names, as listxattr(2); with @buf NULL, just the length.
 * trusted.* only with @show_trusted - the kernel lists those only to
 * CAP_SYS_ADMIN, and decides that from the caller, which we can't see.
 */
int rust_fuse_xattr_list(struct bch_fs *c, subvol_inum inum, char *buf, size_t size,
		    bool show_trusted)
{
	CLASS(btree_trans, trans)(c);
	size_t used = 0;

	int ret = for_each_btree_key_in_subvolume_max(trans, iter, BTREE_ID_xattrs,
				POS(inum.inum, 0), POS(inum.inum, U64_MAX),
				inum.subvol, 0, k, ({
		if (k.k->type != KEY_TYPE_xattr)
			continue;

		struct bkey_s_c_xattr x = bkey_s_c_to_xattr(k);
		unsigned type = x.v->x_type;
		const char *prefix = type < ARRAY_SIZE(rust_fuse_xattr_prefixes)
			? rust_fuse_xattr_prefixes[type] : NULL;

		if (!prefix || (type == KEY_TYPE_XATTR_INDEX_TRUSTED && !show_trusted))
			continue;

		rust_fuse_xattr_emit(buf, size, &used, prefix,
				x.v->x_name_and_value, x.v->x_name_len);
	}));

	return ret ?: used;
}

/*
 * Set, or with @value NULL remove - bch2_xattr_set(), which also updates ctime.
 * @flags: XATTR_CREATE, XATTR_REPLACE; removing passes XATTR_REPLACE, as the
 * VFS does, so that a missing xattr is ENODATA.
 */
int rust_fuse_xattr_set(struct bch_fs *c, subvol_inum inum, const char *name,
		   const void *value, size_t size, int flags)
{
	int type = rust_fuse_xattr_type(&name);
	if (type < 0)
		return type;

	struct bch_inode_unpacked inode_u;
	CLASS(btree_trans, trans)(c);
	return commit_do(trans, NULL, NULL, 0,
		bch2_xattr_set(trans, inum, &inode_u, name, value, size, type, flags));
}
#endif /* BCACHEFS_FUSE */
