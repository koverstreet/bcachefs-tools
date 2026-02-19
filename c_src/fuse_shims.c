// SPDX-License-Identifier: GPL-2.0
//
// C shims for the Rust FUSE mount command. Wraps inline kernel functions
// and complex operations (transactions, closures, bio I/O) that can't be
// called directly from Rust via bindgen.

#ifdef BCACHEFS_FUSE

#include <errno.h>
#include <string.h>

#include "libbcachefs.h"
#include "libbcachefs/bcachefs.h"
#include "libbcachefs/fs/dirent.h"
#include "libbcachefs/fs/namei.h"
#include "libbcachefs/fs/inode.h"
#include "libbcachefs/alloc/accounting.h"
#include "libbcachefs/alloc/buckets.h"
#include "libbcachefs/alloc/foreground.h"
#include "libbcachefs/data/read.h"
#include "libbcachefs/data/write.h"
#include "libbcachefs/btree/iter.h"
#include "libbcachefs/init/fs.h"

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

/* ---- inline function wrappers ---- */

u32 rust_block_bytes(struct bch_fs *c)
{
	return block_bytes(c);
}

struct timespec64 rust_bch2_time_to_timespec(struct bch_fs *c, s64 time)
{
	return bch2_time_to_timespec(c, time);
}

s64 rust_timespec_to_bch2_time(struct bch_fs *c, struct timespec64 ts)
{
	return timespec_to_bch2_time(c, ts);
}

s64 rust_bch2_current_time(struct bch_fs *c)
{
	return bch2_current_time(c);
}

u32 rust_inode_nlink_get(struct bch_inode_unpacked *bi)
{
	return bch2_inode_nlink_get(bi);
}

/* ---- FUSE operations ---- */

int rust_fuse_lookup(struct bch_fs *c, subvol_inum dir,
		     const unsigned char *name, unsigned name_len,
		     subvol_inum *inum_out,
		     struct bch_inode_unpacked *inode_out)
{
	struct bch_inode_unpacked dir_u;
	int ret = bch2_inode_find_by_inum(c, dir, &dir_u);
	if (ret)
		return ret;

	struct bch_hash_info hash_info;
	ret = bch2_hash_info_init(c, &dir_u, &hash_info);
	if (ret)
		return ret;

	struct qstr qstr = QSTR_INIT(name, name_len);
	ret = bch2_dirent_lookup(c, dir, &hash_info, &qstr, inum_out);
	if (ret)
		return ret;

	return bch2_inode_find_by_inum(c, *inum_out, inode_out);
}

int rust_fuse_create(struct bch_fs *c, subvol_inum dir,
		     const unsigned char *name, unsigned name_len,
		     u16 mode, u64 rdev,
		     struct bch_inode_unpacked *new_inode)
{
	struct qstr qstr = QSTR_INIT(name, name_len);
	struct bch_inode_unpacked dir_u;
	uid_t uid = 0;
	gid_t gid = 0;

	bch2_inode_init_early(c, new_inode);

	return bch2_trans_commit_do(c, NULL, NULL, 0,
		bch2_create_trans(trans,
			dir, &dir_u,
			new_inode, &qstr,
			uid, gid, mode, rdev, NULL, NULL,
			(subvol_inum) { 0 }, 0));
}

int rust_fuse_unlink(struct bch_fs *c, subvol_inum dir,
		     const unsigned char *name, unsigned name_len)
{
	struct qstr qstr = QSTR_INIT(name, name_len);
	struct bch_inode_unpacked dir_u, inode_u;

	return bch2_trans_commit_do(c, NULL, NULL,
			BCH_TRANS_COMMIT_no_enospc,
		bch2_unlink_trans(trans, dir, &dir_u,
				  (subvol_inum) {}, &inode_u,
				  &qstr, false));
}

int rust_fuse_rename(struct bch_fs *c,
		     subvol_inum src_dir, const unsigned char *src_name,
		     unsigned src_len,
		     subvol_inum dst_dir, const unsigned char *dst_name,
		     unsigned dst_len)
{
	struct qstr src_qstr = QSTR_INIT(src_name, src_len);
	struct qstr dst_qstr = QSTR_INIT(dst_name, dst_len);
	struct bch_inode_unpacked src_dir_u, dst_dir_u;
	struct bch_inode_unpacked src_inode_u, dst_inode_u;

	/* XXX handle overwrites */
	return bch2_trans_commit_do(c, NULL, NULL, 0,
		bch2_rename_trans(trans,
			src_dir, &src_dir_u,
			dst_dir, &dst_dir_u,
			&src_inode_u, &dst_inode_u,
			&src_qstr, &dst_qstr,
			BCH_RENAME));
}

int rust_fuse_link(struct bch_fs *c, subvol_inum inum, subvol_inum newparent,
		   const unsigned char *name, unsigned name_len,
		   struct bch_inode_unpacked *inode_u)
{
	struct qstr qstr = QSTR_INIT(name, name_len);
	struct bch_inode_unpacked dir_u;

	return bch2_trans_commit_do(c, NULL, NULL, 0,
		bch2_link_trans(trans, newparent, &dir_u,
				inum, inode_u, &qstr));
}

int rust_fuse_setattr(struct bch_fs *c, subvol_inum inum,
		      struct bch_inode_unpacked *inode_out,
		      int set_mode, u16 mode,
		      int set_uid, u32 uid,
		      int set_gid, u32 gid,
		      int set_size, u64 size,
		      int atime_flag, s64 atime,
		      int mtime_flag, s64 mtime)
{
	CLASS(btree_trans, trans)(c);
	return commit_do(trans, NULL, NULL, BCH_TRANS_COMMIT_no_enospc, ({
		u64 now = bch2_current_time(c);

		CLASS(btree_iter_uninit, iter)(trans);
		struct bch_inode_unpacked inode_u;
		int ret2 = bch2_inode_peek(trans, &iter, &inode_u, inum,
					    BTREE_ITER_intent);
		if (ret2)
			goto setattr_err;

		if (set_mode)
			inode_u.bi_mode = mode;
		if (set_uid)
			inode_u.bi_uid = uid;
		if (set_gid)
			inode_u.bi_gid = gid;
		if (set_size)
			inode_u.bi_size = size;
		if (atime_flag == 1)
			inode_u.bi_atime = atime;
		if (atime_flag == 2)
			inode_u.bi_atime = now;
		if (mtime_flag == 1)
			inode_u.bi_mtime = mtime;
		if (mtime_flag == 2)
			inode_u.bi_mtime = now;

		*inode_out = inode_u;
setattr_err:
		ret2 ?:
		bch2_inode_write(trans, &iter, &inode_u);
	}));
}

/* ---- read/write I/O ---- */

static void fuse_read_endio(struct bio *bio)
{
	closure_put(bio->bi_private);
}

int rust_fuse_read_aligned(struct bch_fs *c, subvol_inum inum,
			   size_t aligned_size, loff_t aligned_offset,
			   void *buf)
{
	struct bch_inode_unpacked inode;
	if (bch2_inode_find_by_inum(c, inum, &inode))
		return -ENOENT;

	struct bch_inode_opts io_opts;
	bch2_inode_opts_get_inode(c, &inode, &io_opts);

	struct bch_read_bio rbio;
	struct bio_vec bv;
	bio_init(&rbio.bio, NULL, &bv, 1, 0);
	bio_add_virt_nofail(&rbio.bio, buf, aligned_size);
	bio_set_op_attrs(&rbio.bio, REQ_OP_READ, REQ_SYNC);
	rbio.bio.bi_iter.bi_sector = aligned_offset >> 9;

	struct closure cl;
	closure_init_stack(&cl);

	closure_get(&cl);
	rbio.bio.bi_private = &cl;

	bch2_read(c, rbio_init(&rbio.bio, c, io_opts, fuse_read_endio), inum);

	closure_sync(&cl);

	return -blk_status_to_errno(rbio.bio.bi_status);
}

struct fuse_write_op {
	struct closure		cl;
	/* must be last: */
	struct bch_write_op	op;
};

static void fuse_write_endio(struct bch_write_op *op)
{
	struct fuse_write_op *w = container_of(op, struct fuse_write_op, op);
	closure_put(&w->cl);
}

int rust_fuse_write(struct bch_fs *c, subvol_inum inum,
		    const void *data, size_t size, loff_t offset,
		    size_t *written_out)
{
	*written_out = 0;

	struct bch_inode_unpacked inode;
	if (bch2_inode_find_by_inum(c, inum, &inode))
		return -ENOENT;

	struct bch_inode_opts io_opts;
	bch2_inode_opts_get_inode(c, &inode, &io_opts);

	u32 bs = block_bytes(c);
	loff_t aligned_start = round_down(offset, bs);
	size_t pad_start = offset - aligned_start;
	loff_t end = offset + size;
	loff_t aligned_end = round_up(end, bs);
	size_t pad_end = aligned_end - end;
	size_t aligned_size = aligned_end - aligned_start;

	void *aligned_buf = aligned_alloc(PAGE_SIZE, aligned_size);
	if (!aligned_buf)
		return -ENOMEM;

	int ret = 0;

	/* Read partial start block */
	if (pad_start) {
		memset(aligned_buf, 0, bs);
		ret = rust_fuse_read_aligned(c, inum, bs, aligned_start, aligned_buf);
		if (ret)
			goto out;
	}

	/* Read partial end block (if different from start) */
	if (pad_end && !(pad_start && aligned_size == (size_t)bs)) {
		loff_t partial_end_start = aligned_end - bs;
		size_t buf_offset = aligned_size - bs;
		memset(aligned_buf + buf_offset, 0, bs);
		ret = rust_fuse_read_aligned(c, inum, bs, partial_end_start,
					     aligned_buf + buf_offset);
		if (ret)
			goto out;
	}

	/* Overlay write data */
	memcpy(aligned_buf + pad_start, data, size);

	/* Do the write */
	struct fuse_write_op w = {};
	struct bch_write_op *op = &w.op;
	struct bio_vec bv;

	closure_init_stack(&w.cl);

	bch2_write_op_init(op, c, io_opts);
	op->write_point	= writepoint_hashed(0);
	op->nr_replicas	= io_opts.data_replicas;
	op->target	= io_opts.foreground_target;
	op->subvol	= inum.subvol;
	op->pos		= POS(inum.inum, aligned_start >> 9);
	op->new_i_size	= offset + size;
	op->end_io	= fuse_write_endio;

	bio_init(&op->wbio.bio, NULL, &bv, 1, 0);
	bio_add_virt_nofail(&op->wbio.bio, aligned_buf, aligned_size);
	bio_set_op_attrs(&op->wbio.bio, REQ_OP_WRITE, REQ_SYNC);

	if (bch2_disk_reservation_get(c, &op->res, aligned_size >> 9,
				      op->nr_replicas, 0)) {
		ret = -ENOSPC;
		goto out;
	}

	closure_get(&w.cl);
	closure_call(&op->cl, bch2_write, NULL, NULL);
	closure_sync(&w.cl);

	if (!op->error) {
		size_t aligned_written = op->written << 9;
		/* Convert aligned bytes written back to unaligned */
		size_t bytes = 0;
		if (aligned_written > pad_start) {
			bytes = aligned_written - pad_start;
			if (bytes > pad_end)
				bytes -= pad_end;
			else
				bytes = 0;
		}
		*written_out = bytes;
	}
	ret = op->error;

	/* Update inode times */
	if (!ret) {
		CLASS(btree_trans, trans)(c);
		ret = commit_do(trans, NULL, NULL, BCH_TRANS_COMMIT_no_enospc, ({
			u64 now = bch2_current_time(c);
			CLASS(btree_iter_uninit, iter)(trans);
			struct bch_inode_unpacked inode_u;
			int ret2 = bch2_inode_peek(trans, &iter, &inode_u, inum,
						    BTREE_ITER_intent);
			if (ret2)
				goto time_err;
			inode_u.bi_mtime = now;
			inode_u.bi_ctime = now;
time_err:
			ret2 ?:
			bch2_inode_write(trans, &iter, &inode_u);
		}));
	}

out:
	free(aligned_buf);
	return ret;
}

int rust_fuse_symlink(struct bch_fs *c, subvol_inum dir,
		      const unsigned char *name, unsigned name_len,
		      const unsigned char *link, unsigned link_len,
		      struct bch_inode_unpacked *new_inode)
{
	int ret;

	ret = rust_fuse_create(c, dir, name, name_len,
			       S_IFLNK|S_IRWXUGO, 0, new_inode);
	if (ret)
		return ret;

	subvol_inum inum = { dir.subvol, new_inode->bi_inum };
	size_t written = 0;
	ret = rust_fuse_write(c, inum, link, link_len + 1, 0, &written);
	if (ret)
		return ret;

	new_inode->bi_size = written;
	return 0;
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

	return bch2_readdir(c, dir, &dir_hash, &rctx.ctx);
}

/* ---- statfs ---- */

struct bch_fs_usage_short rust_bch2_fs_usage_read_short(struct bch_fs *c)
{
	return bch2_fs_usage_read_short(c);
}

void rust_fuse_count_inodes(struct bch_fs *c, u64 *out)
{
	struct disk_accounting_pos k;
	disk_accounting_key_init(k, nr_inodes);
	bch2_accounting_mem_read(c, disk_accounting_pos_to_bpos(&k), out, 1);
}

#endif /* BCACHEFS_FUSE */
