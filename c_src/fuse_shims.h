/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _FUSE_SHIMS_H
#define _FUSE_SHIMS_H

#include "fs/bcachefs.h"
#include "fs/fs/inode.h"
#include "fs/alloc/buckets.h"

/*
 * C shims for the Rust FUSE mount command.
 *
 * These wrap kernel operations that use inline functions, macros,
 * or complex types (qstr, btree_trans, closures) that can't be
 * expressed through bindgen.
 */

/* Thread initialization — must be called on fuser worker threads */
void rust_fuse_ensure_current(void);
void rust_fuse_rcu_register(void);
void rust_fuse_rcu_unregister(void);

/* Directory reading */
typedef int (*rust_fuse_filldir_fn)(void *ctx,
				    const char *name, unsigned name_len,
				    u64 ino, unsigned type, u64 pos);

int rust_fuse_readdir(struct bch_fs *c, subvol_inum dir,
		      u64 pos, void *ctx, rust_fuse_filldir_fn filldir);

/* Accounting */
struct bch_fs_usage_short rust_bch2_fs_usage_read_short(struct bch_fs *c);
void rust_fuse_count_inodes(struct bch_fs *c, u64 *nr_inodes);

#endif /* _FUSE_SHIMS_H */
