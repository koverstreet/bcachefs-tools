/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _FUSE_SHIMS_H
#define _FUSE_SHIMS_H

#include "libbcachefs/bcachefs.h"
#include "libbcachefs/fs/inode.h"
#include "libbcachefs/alloc/buckets.h"

/*
 * C shims for the Rust FUSE mount command.
 *
 * These wrap kernel operations that use inline functions, macros,
 * or complex types (qstr, btree_trans, closures) that can't be
 * expressed through bindgen.
 */

/* Inline function wrappers */
u32 rust_block_bytes(struct bch_fs *c);
u32 rust_inode_nlink_get(struct bch_inode_unpacked *bi);
struct timespec64 rust_bch2_time_to_timespec(struct bch_fs *c, s64 time);
s64 rust_timespec_to_bch2_time(struct bch_fs *c, struct timespec64 ts);
s64 rust_bch2_current_time(struct bch_fs *c);

/* Filesystem operations */

int rust_fuse_lookup(struct bch_fs *c, subvol_inum dir,
		     const unsigned char *name, unsigned name_len,
		     subvol_inum *inum_out,
		     struct bch_inode_unpacked *inode_out);

int rust_fuse_create(struct bch_fs *c, subvol_inum dir,
		     const unsigned char *name, unsigned name_len,
		     u16 mode, u64 rdev,
		     struct bch_inode_unpacked *new_inode);

int rust_fuse_unlink(struct bch_fs *c, subvol_inum dir,
		     const unsigned char *name, unsigned name_len);

int rust_fuse_rename(struct bch_fs *c,
		     subvol_inum src_dir, const unsigned char *src_name,
		     unsigned src_len,
		     subvol_inum dst_dir, const unsigned char *dst_name,
		     unsigned dst_len);

int rust_fuse_link(struct bch_fs *c, subvol_inum inum,
		   subvol_inum newparent, const unsigned char *name,
		   unsigned name_len,
		   struct bch_inode_unpacked *inode_out);

int rust_fuse_symlink(struct bch_fs *c, subvol_inum dir,
		      const unsigned char *name, unsigned name_len,
		      const unsigned char *link, unsigned link_len,
		      struct bch_inode_unpacked *new_inode);

int rust_fuse_setattr(struct bch_fs *c, subvol_inum inum,
		      struct bch_inode_unpacked *inode_out,
		      int set_mode, u16 mode,
		      int set_uid, u32 uid,
		      int set_gid, u32 gid,
		      int set_size, u64 size,
		      int atime_flag, s64 atime,
		      int mtime_flag, s64 mtime);

/* I/O operations */
int rust_fuse_read_aligned(struct bch_fs *c, subvol_inum inum,
			   size_t aligned_size, loff_t aligned_offset,
			   void *buf);

int rust_fuse_write(struct bch_fs *c, subvol_inum inum,
		    const void *buf, size_t size, loff_t offset,
		    size_t *written_out);

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
