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

#include <linux/dcache.h>

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

	return bch2_readdir(c, dir, &dir_hash, &rctx.ctx, NULL);
}

#endif /* BCACHEFS_FUSE */
