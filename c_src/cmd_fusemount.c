#ifdef BCACHEFS_FUSE

#include <errno.h>
#include <float.h>
#include <getopt.h>
#include <stdio.h>
#include <sys/statvfs.h>

#include <fuse_lowlevel.h>

#include "cmds.h"
#include "libbcachefs.h"
#include "tools-util.h"

#include "libbcachefs/bcachefs.h"
#include "libbcachefs/alloc_foreground.h"
#include "libbcachefs/btree_iter.h"
#include "libbcachefs/buckets.h"
#include "libbcachefs/dirent.h"
#include "libbcachefs/errcode.h"
#include "libbcachefs/error.h"
#include "libbcachefs/fs-common.h"
#include "libbcachefs/inode.h"
#include "libbcachefs/io_read.h"
#include "libbcachefs/io_write.h"
#include "libbcachefs/opts.h"
#include "libbcachefs/super.h"

/* mode_to_type(): */
#include "libbcachefs/fs.h"

#include <linux/dcache.h>

/* XXX cut and pasted from fsck.c */
#define QSTR(n) { { { .len = strlen(n) } }, .name = n }

/* used by write_aligned function for waiting on bch2_write closure */
struct write_aligned_op_t {
        struct closure cl;

        /* must be last: */
        struct bch_write_op             op;
};


static inline subvol_inum map_root_ino(u64 ino)
{
	return (subvol_inum) { 1, ino == 1 ? 4096 : ino };
}

static inline u64 unmap_root_ino(u64 ino)
{
	return ino == 4096 ? 1 : ino;
}

static struct stat inode_to_stat(struct bch_fs *c,
				 struct bch_inode_unpacked *bi)
{
	return (struct stat) {
		.st_ino		= unmap_root_ino(bi->bi_inum),
		.st_size	= bi->bi_size,
		.st_mode	= bi->bi_mode,
		.st_uid		= bi->bi_uid,
		.st_gid		= bi->bi_gid,
		.st_nlink	= bch2_inode_nlink_get(bi),
		.st_rdev	= bi->bi_dev,
		.st_blksize	= block_bytes(c),
		.st_blocks	= bi->bi_sectors,
		.st_atim	= bch2_time_to_timespec(c, bi->bi_atime),
		.st_mtim	= bch2_time_to_timespec(c, bi->bi_mtime),
		.st_ctim	= bch2_time_to_timespec(c, bi->bi_ctime),
	};
}

static struct fuse_entry_param inode_to_entry(struct bch_fs *c,
					      struct bch_inode_unpacked *bi)
{
	return (struct fuse_entry_param) {
		.ino		= unmap_root_ino(bi->bi_inum),
		.generation	= bi->bi_generation,
		.attr		= inode_to_stat(c, bi),
		.attr_timeout	= DBL_MAX,
		.entry_timeout	= DBL_MAX,
	};
}

static void bcachefs_fuse_init(void *arg, struct fuse_conn_info *conn)
{
	if (conn->capable & FUSE_CAP_WRITEBACK_CACHE) {
		fuse_log(FUSE_LOG_DEBUG, "fuse_init: activating writeback\n");
		conn->want |= FUSE_CAP_WRITEBACK_CACHE;
	} else
		fuse_log(FUSE_LOG_DEBUG, "fuse_init: writeback not capable\n");

	//conn->want |= FUSE_CAP_POSIX_ACL;
}

static void bcachefs_fuse_destroy(void *arg)
{
	struct bch_fs *c = arg;

	bch2_fs_stop(c);
}

static void bcachefs_fuse_lookup(fuse_req_t req, fuse_ino_t dir_ino,
				 const char *name)
{
	subvol_inum dir = map_root_ino(dir_ino);
	struct bch_fs *c = fuse_req_userdata(req);
	struct bch_inode_unpacked bi;
	struct qstr qstr = QSTR(name);
	subvol_inum inum;
	int ret;

	fuse_log(FUSE_LOG_DEBUG, "fuse_lookup(dir=%llu name=%s)\n",
		 dir.inum, name);

	ret = bch2_inode_find_by_inum(c, dir, &bi);
	if (ret) {
		fuse_reply_err(req, -ret);
		return;
	}

	struct bch_hash_info hash_info = bch2_hash_info_init(c, &bi);

	ret = bch2_dirent_lookup(c, dir, &hash_info, &qstr, &inum);
	if (ret) {
		struct fuse_entry_param e = {
			.attr_timeout	= DBL_MAX,
			.entry_timeout	= DBL_MAX,
		};
		fuse_reply_entry(req, &e);
		return;
	}

	ret = bch2_inode_find_by_inum(c, inum, &bi);
	if (ret)
		goto err;

	fuse_log(FUSE_LOG_DEBUG, "fuse_lookup ret(inum=%llu)\n",
		 bi.bi_inum);

	struct fuse_entry_param e = inode_to_entry(c, &bi);
	fuse_reply_entry(req, &e);
	return;
err:
	fuse_log(FUSE_LOG_DEBUG, "fuse_lookup error %i\n", ret);
	fuse_reply_err(req, -ret);
}

static void bcachefs_fuse_getattr(fuse_req_t req, fuse_ino_t ino,
				  struct fuse_file_info *fi)
{
	subvol_inum inum = map_root_ino(ino);
	struct bch_fs *c = fuse_req_userdata(req);
	struct bch_inode_unpacked bi;
	struct stat attr;

	fuse_log(FUSE_LOG_DEBUG, "fuse_getattr(inum=%llu)\n", inum.inum);

	int ret = bch2_inode_find_by_inum(c, inum, &bi);
	if (ret) {
		fuse_log(FUSE_LOG_DEBUG, "fuse_getattr error %i\n", ret);
		fuse_reply_err(req, -ret);
		return;
	}

	fuse_log(FUSE_LOG_DEBUG, "fuse_getattr success\n");

	attr = inode_to_stat(c, &bi);
	fuse_reply_attr(req, &attr, DBL_MAX);
}

static void bcachefs_fuse_setattr(fuse_req_t req, fuse_ino_t ino,
				  struct stat *attr, int to_set,
				  struct fuse_file_info *fi)
{
	struct bch_fs *c = fuse_req_userdata(req);
	struct bch_inode_unpacked inode_u;
	struct btree_trans *trans;
	struct btree_iter iter;
	u64 now;
	int ret;

	subvol_inum inum = map_root_ino(ino);

	fuse_log(FUSE_LOG_DEBUG, "bcachefs_fuse_setattr(%llu, %x)\n", inum.inum, to_set);

	trans = bch2_trans_get(c);
retry:
	bch2_trans_begin(trans);
	now = bch2_current_time(c);

	ret = bch2_inode_peek(trans, &iter, &inode_u, inum, BTREE_ITER_intent);
	if (ret)
		goto err;

	if (to_set & FUSE_SET_ATTR_MODE)
		inode_u.bi_mode	= attr->st_mode;
	if (to_set & FUSE_SET_ATTR_UID)
		inode_u.bi_uid	= attr->st_uid;
	if (to_set & FUSE_SET_ATTR_GID)
		inode_u.bi_gid	= attr->st_gid;
	if (to_set & FUSE_SET_ATTR_SIZE)
		inode_u.bi_size	= attr->st_size;
	if (to_set & FUSE_SET_ATTR_ATIME)
		inode_u.bi_atime = timespec_to_bch2_time(c, attr->st_atim);
	if (to_set & FUSE_SET_ATTR_MTIME)
		inode_u.bi_mtime = timespec_to_bch2_time(c, attr->st_mtim);
	if (to_set & FUSE_SET_ATTR_ATIME_NOW)
		inode_u.bi_atime = now;
	if (to_set & FUSE_SET_ATTR_MTIME_NOW)
		inode_u.bi_mtime = now;
	/* TODO: CTIME? */

	ret   = bch2_inode_write(trans, &iter, &inode_u) ?:
		bch2_trans_commit(trans, NULL, NULL,
				  BCH_TRANS_COMMIT_no_enospc);
err:
        bch2_trans_iter_exit(trans, &iter);
	if (ret == -EINTR)
		goto retry;

	bch2_trans_put(trans);

	if (!ret) {
		*attr = inode_to_stat(c, &inode_u);
		fuse_reply_attr(req, attr, DBL_MAX);
	} else {
		fuse_reply_err(req, -ret);
	}
}

static int do_create(struct bch_fs *c, subvol_inum dir,
		     const char *name, mode_t mode, dev_t rdev,
		     struct bch_inode_unpacked *new_inode)
{
	struct qstr qstr = QSTR(name);
	struct bch_inode_unpacked dir_u;
	uid_t uid = 0;
	gid_t gid = 0;

	bch2_inode_init_early(c, new_inode);

	return bch2_trans_do(c, NULL, NULL, 0,
			bch2_create_trans(trans,
				dir, &dir_u,
				new_inode, &qstr,
				uid, gid, mode, rdev, NULL, NULL,
				(subvol_inum) { 0 }, 0));
}

static void bcachefs_fuse_mknod(fuse_req_t req, fuse_ino_t dir_ino,
				const char *name, mode_t mode,
				dev_t rdev)
{
	subvol_inum dir = map_root_ino(dir_ino);
	struct bch_fs *c = fuse_req_userdata(req);
	struct bch_inode_unpacked new_inode;
	int ret;

	fuse_log(FUSE_LOG_DEBUG, "bcachefs_fuse_mknod(%llu, %s, %x, %x)\n",
		 dir.inum, name, mode, rdev);

	ret = do_create(c, dir, name, mode, rdev, &new_inode);
	if (ret)
		goto err;

	struct fuse_entry_param e = inode_to_entry(c, &new_inode);
	fuse_reply_entry(req, &e);
	return;
err:
	fuse_reply_err(req, -ret);
}

static void bcachefs_fuse_mkdir(fuse_req_t req, fuse_ino_t dir,
				const char *name, mode_t mode)
{
	fuse_log(FUSE_LOG_DEBUG, "bcachefs_fuse_mkdir(%llu, %s, %x)\n",
		 dir, name, mode);

	BUG_ON(mode & S_IFMT);

	mode |= S_IFDIR;
	bcachefs_fuse_mknod(req, dir, name, mode, 0);
}

static void bcachefs_fuse_unlink(fuse_req_t req, fuse_ino_t dir_ino,
				 const char *name)
{
	struct bch_fs *c = fuse_req_userdata(req);
	struct bch_inode_unpacked dir_u, inode_u;
	struct qstr qstr = QSTR(name);
	subvol_inum dir = map_root_ino(dir_ino);

	fuse_log(FUSE_LOG_DEBUG, "bcachefs_fuse_unlink(%llu, %s)\n", dir.inum, name);

	int ret = bch2_trans_do(c, NULL, NULL,
				BCH_TRANS_COMMIT_no_enospc,
			    bch2_unlink_trans(trans, dir, &dir_u,
					      &inode_u, &qstr, false));

	fuse_reply_err(req, -ret);
}

static void bcachefs_fuse_rmdir(fuse_req_t req, fuse_ino_t dir,
				const char *name)
{
	fuse_log(FUSE_LOG_DEBUG, "bcachefs_fuse_rmdir(%llu, %s)\n", dir, name);

	bcachefs_fuse_unlink(req, dir, name);
}

static void bcachefs_fuse_rename(fuse_req_t req,
				 fuse_ino_t src_dir_ino, const char *srcname,
				 fuse_ino_t dst_dir_ino, const char *dstname,
				 unsigned flags)
{
	struct bch_fs *c = fuse_req_userdata(req);
	struct bch_inode_unpacked dst_dir_u, src_dir_u;
	struct bch_inode_unpacked src_inode_u, dst_inode_u;
	struct qstr dst_name = QSTR(srcname);
	struct qstr src_name = QSTR(dstname);
	subvol_inum src_dir = map_root_ino(src_dir_ino);
	subvol_inum dst_dir = map_root_ino(dst_dir_ino);
	int ret;

	fuse_log(FUSE_LOG_DEBUG,
		 "bcachefs_fuse_rename(%llu, %s, %llu, %s, %x)\n",
		 src_dir.inum, srcname, dst_dir.inum, dstname, flags);

	/* XXX handle overwrites */
	ret = bch2_trans_do(c, NULL, NULL, 0,
		bch2_rename_trans(trans,
				  src_dir, &src_dir_u,
				  dst_dir, &dst_dir_u,
				  &src_inode_u, &dst_inode_u,
				  &src_name, &dst_name,
				  BCH_RENAME));

	fuse_reply_err(req, -ret);
}

static void bcachefs_fuse_link(fuse_req_t req, fuse_ino_t ino,
			       fuse_ino_t newparent_ino, const char *newname)
{
	struct bch_fs *c = fuse_req_userdata(req);
	struct bch_inode_unpacked dir_u, inode_u;
	struct qstr qstr = QSTR(newname);
	subvol_inum newparent	= map_root_ino(newparent_ino);
	subvol_inum inum	= map_root_ino(ino);
	int ret;

	fuse_log(FUSE_LOG_DEBUG, "bcachefs_fuse_link(%llu, %llu, %s)\n",
		 inum.inum, newparent.inum, newname);

	ret = bch2_trans_do(c, NULL, NULL, 0,
			    bch2_link_trans(trans, newparent, &dir_u,
					    inum, &inode_u, &qstr));

	if (!ret) {
		struct fuse_entry_param e = inode_to_entry(c, &inode_u);
		fuse_reply_entry(req, &e);
	} else {
		fuse_reply_err(req, -ret);
	}
}

static void bcachefs_fuse_open(fuse_req_t req, fuse_ino_t inum,
			       struct fuse_file_info *fi)
{
	fi->direct_io		= false;
	fi->keep_cache		= true;
	fi->cache_readdir	= true;

	fuse_reply_open(req, fi);
}

static void userbio_init(struct bio *bio, struct bio_vec *bv,
			 void *buf, size_t size)
{
	bio_init(bio, NULL, bv, 1, 0);
	bio->bi_iter.bi_size	= size;
	bv->bv_page		= buf;
	bv->bv_len		= size;
	bv->bv_offset		= 0;
}

static int get_inode_io_opts(struct bch_fs *c, subvol_inum inum, struct bch_io_opts *opts)
{
	struct bch_inode_unpacked inode;
	if (bch2_inode_find_by_inum(c, inum, &inode))
		return -EINVAL;

	bch2_inode_opts_get(opts, c, &inode);
	return 0;
}

static void bcachefs_fuse_read_endio(struct bio *bio)
{
	closure_put(bio->bi_private);
}


static void bcachefs_fuse_write_endio(struct bch_write_op *op)
{
       struct write_aligned_op_t *w = container_of(op,struct write_aligned_op_t,op);
       closure_put(&w->cl);
}


struct fuse_align_io {
	off_t		start;
	size_t		pad_start;
	off_t		end;
	size_t		pad_end;
	size_t		size;
};

/* Handle unaligned start and end */
/* TODO: align to block_bytes, sector size, or page size? */
static struct fuse_align_io align_io(const struct bch_fs *c, size_t size,
				     off_t offset)
{
	struct fuse_align_io align;

	BUG_ON(offset < 0);

	align.start = round_down(offset, block_bytes(c));
	align.pad_start = offset - align.start;

	off_t end = offset + size;
	align.end = round_up(end, block_bytes(c));
	align.pad_end = align.end - end;

	align.size = align.end - align.start;

	return align;
}

/*
 * Given an aligned number of bytes transferred, figure out how many unaligned
 * bytes were transferred.
 */
static size_t align_fix_up_bytes(const struct fuse_align_io *align,
				 size_t align_bytes)
{
	size_t bytes = 0;

	if (align_bytes > align->pad_start) {
		bytes = align_bytes - align->pad_start;
		bytes = bytes > align->pad_end ? bytes - align->pad_end : 0;
	}

	return bytes;
}

/*
 * Read aligned data.
 */
static int read_aligned(struct bch_fs *c, subvol_inum inum, size_t aligned_size,
			off_t aligned_offset, void *buf)
{
	BUG_ON(aligned_size & (block_bytes(c) - 1));
	BUG_ON(aligned_offset & (block_bytes(c) - 1));

	struct bch_io_opts io_opts;
	if (get_inode_io_opts(c, inum, &io_opts))
		return -ENOENT;

	struct bch_read_bio rbio;
	struct bio_vec bv;
	userbio_init(&rbio.bio, &bv, buf, aligned_size);
	bio_set_op_attrs(&rbio.bio, REQ_OP_READ, REQ_SYNC);
	rbio.bio.bi_iter.bi_sector	= aligned_offset >> 9;

	struct closure cl;
	closure_init_stack(&cl);

	closure_get(&cl);
	rbio.bio.bi_end_io		= bcachefs_fuse_read_endio;
	rbio.bio.bi_private		= &cl;

	bch2_read(c, rbio_init(&rbio.bio, io_opts), inum);

	closure_sync(&cl);

	return -blk_status_to_errno(rbio.bio.bi_status);
}

static void bcachefs_fuse_read(fuse_req_t req, fuse_ino_t ino,
			       size_t size, off_t offset,
			       struct fuse_file_info *fi)
{
	subvol_inum inum = map_root_ino(ino);
	struct bch_fs *c = fuse_req_userdata(req);

	fuse_log(FUSE_LOG_DEBUG, "bcachefs_fuse_read(%llu, %zd, %lld)\n",
		 inum, size, offset);

	/* Check inode size. */
	struct bch_inode_unpacked bi;
	int ret = bch2_inode_find_by_inum(c, inum, &bi);
	if (ret) {
		fuse_reply_err(req, -ret);
		return;
	}

	off_t end = min_t(u64, bi.bi_size, offset + size);
	if (end <= offset) {
		fuse_reply_buf(req, NULL, 0);
		return;
	}
	size = end - offset;

	struct fuse_align_io align = align_io(c, size, offset);

	void *buf = aligned_alloc(PAGE_SIZE, align.size);
	if (!buf) {
		fuse_reply_err(req, ENOMEM);
		return;
	}

	ret = read_aligned(c, inum, align.size, align.start, buf);

	if (likely(!ret))
		fuse_reply_buf(req, buf + align.pad_start, size);
	else
		fuse_reply_err(req, -ret);

	free(buf);
}

static int inode_update_times(struct bch_fs *c, subvol_inum inum)
{
	struct btree_trans *trans;
	struct btree_iter iter;
	struct bch_inode_unpacked inode_u;
	int ret = 0;
	u64 now;

	trans = bch2_trans_get(c);
retry:
	bch2_trans_begin(trans);
	now = bch2_current_time(c);

	ret = bch2_inode_peek(trans, &iter, &inode_u, inum, BTREE_ITER_intent);
	if (ret)
		goto err;

	inode_u.bi_mtime = now;
	inode_u.bi_ctime = now;

	ret = bch2_inode_write(trans, &iter, &inode_u);
	if (ret)
		goto err;

	ret = bch2_trans_commit(trans, NULL, NULL,
				BCH_TRANS_COMMIT_no_enospc);
err:
        bch2_trans_iter_exit(trans, &iter);
	if (ret == -EINTR)
		goto retry;

	bch2_trans_put(trans);
	return ret;
}

static int write_aligned(struct bch_fs *c, subvol_inum inum,
			 struct bch_io_opts io_opts, void *buf,
			 size_t aligned_size, off_t aligned_offset,
			 off_t new_i_size, size_t *written_out)
{

	struct write_aligned_op_t w = { 0 }
;
	struct bch_write_op	*op = &w.op;
	struct bio_vec		bv;

	BUG_ON(aligned_size & (block_bytes(c) - 1));
	BUG_ON(aligned_offset & (block_bytes(c) - 1));

	*written_out = 0;

	closure_init_stack(&w.cl);

	bch2_write_op_init(op, c, io_opts); /* XXX reads from op?! */
	op->write_point	= writepoint_hashed(0);
	op->nr_replicas	= io_opts.data_replicas;
	op->target	= io_opts.foreground_target;
	op->subvol	= inum.subvol;
	op->pos		= POS(inum.inum, aligned_offset >> 9);
	op->new_i_size	= new_i_size;
	op->end_io = bcachefs_fuse_write_endio;

	userbio_init(&op->wbio.bio, &bv, buf, aligned_size);
	bio_set_op_attrs(&op->wbio.bio, REQ_OP_WRITE, REQ_SYNC);

	if (bch2_disk_reservation_get(c, &op->res, aligned_size >> 9,
				      op->nr_replicas, 0)) {
		/* XXX: use check_range_allocated like dio write path */
		return -ENOSPC;
	}

	closure_get(&w.cl);

	closure_call(&op->cl, bch2_write, NULL, NULL);

	closure_sync(&w.cl);

	if (!op->error)
		*written_out = op->written << 9;

	return op->error;
}

static void bcachefs_fuse_write(fuse_req_t req, fuse_ino_t ino,
				const char *buf, size_t size,
				off_t offset,
				struct fuse_file_info *fi)
{
	subvol_inum inum = map_root_ino(ino);
	struct bch_fs *c	= fuse_req_userdata(req);
	struct bch_io_opts	io_opts;
	size_t			aligned_written;
	int			ret = 0;

	fuse_log(FUSE_LOG_DEBUG, "bcachefs_fuse_write(%llu, %zd, %lld)\n",
		 inum, size, offset);

	struct fuse_align_io align = align_io(c, size, offset);
	void *aligned_buf = aligned_alloc(PAGE_SIZE, align.size);
	BUG_ON(!aligned_buf);

	if (get_inode_io_opts(c, inum, &io_opts)) {
		ret = -ENOENT;
		goto err;
	}

	/* Realign the data and read in start and end, if needed */

	/* Read partial start data. */
	if (align.pad_start) {
		memset(aligned_buf, 0, block_bytes(c));

		ret = read_aligned(c, inum, block_bytes(c), align.start,
				   aligned_buf);
		if (ret)
			goto err;
	}

	/*
	 * Read partial end data. If the whole write fits in one block, the
	 * start data and the end data are the same so this isn't needed.
	 */
	if (align.pad_end &&
	    !(align.pad_start && align.size == block_bytes(c))) {
		off_t partial_end_start = align.end - block_bytes(c);
		size_t buf_offset = align.size - block_bytes(c);

		memset(aligned_buf + buf_offset, 0, block_bytes(c));

		ret = read_aligned(c, inum, block_bytes(c), partial_end_start,
				   aligned_buf + buf_offset);
		if (ret)
			goto err;
	}

	/* Overlay what we want to write. */
	memcpy(aligned_buf + align.pad_start, buf, size);

	/* Actually write. */
	ret = write_aligned(c, inum, io_opts, aligned_buf,
			    align.size, align.start,
			    offset + size, &aligned_written);

	/* Figure out how many unaligned bytes were written. */
	size_t written = align_fix_up_bytes(&align, aligned_written);
	BUG_ON(written > size);

	fuse_log(FUSE_LOG_DEBUG, "bcachefs_fuse_write: wrote %zd bytes\n",
		 written);

	if (written > 0)
		ret = 0;

	/*
	 * Update inode times.
	 * TODO: Integrate with bch2_extent_update()
	 */
	if (!ret)
		ret = inode_update_times(c, inum);

	if (!ret) {
		BUG_ON(written == 0);
		fuse_reply_write(req, written);
		free(aligned_buf);
		return;
	}

err:
	fuse_reply_err(req, -ret);
	free(aligned_buf);
}

static void bcachefs_fuse_symlink(fuse_req_t req, const char *link,
				  fuse_ino_t dir_ino, const char *name)
{
	subvol_inum dir = map_root_ino(dir_ino);
	struct bch_fs *c = fuse_req_userdata(req);
	struct bch_inode_unpacked new_inode;
	size_t link_len = strlen(link);
	int ret;

	fuse_log(FUSE_LOG_DEBUG, "bcachefs_fuse_symlink(%s, %llu, %s)\n",
		 link, dir.inum, name);

	ret = do_create(c, dir, name, S_IFLNK|S_IRWXUGO, 0, &new_inode);
	if (ret)
		goto err;

	struct bch_io_opts io_opts;
	ret = get_inode_io_opts(c, dir, &io_opts);
	if (ret)
		goto err;

	struct fuse_align_io align = align_io(c, link_len + 1, 0);

	void *aligned_buf = aligned_alloc(PAGE_SIZE, align.size);
	BUG_ON(!aligned_buf);

	memset(aligned_buf, 0, align.size);
	memcpy(aligned_buf, link, link_len); /* already terminated */

	subvol_inum inum = (subvol_inum) { dir.subvol, new_inode.bi_inum };

	size_t aligned_written;
	ret = write_aligned(c, inum, io_opts, aligned_buf,
			    align.size, align.start, link_len + 1,
			    &aligned_written);
	free(aligned_buf);

	if (ret)
		goto err;

	size_t written = align_fix_up_bytes(&align, aligned_written);
	BUG_ON(written != link_len + 1); // TODO: handle short

	ret = inode_update_times(c, inum);
	if (ret)
		goto err;

	new_inode.bi_size = written;

	struct fuse_entry_param e = inode_to_entry(c, &new_inode);
	fuse_reply_entry(req, &e);
	return;

err:
	fuse_reply_err(req, -ret);
}

static void bcachefs_fuse_readlink(fuse_req_t req, fuse_ino_t ino)
{
	subvol_inum inum = map_root_ino(ino);
	struct bch_fs *c = fuse_req_userdata(req);
	char *buf = NULL;

	fuse_log(FUSE_LOG_DEBUG, "bcachefs_fuse_readlink(%llu)\n", inum.inum);

	struct bch_inode_unpacked bi;
	int ret = bch2_inode_find_by_inum(c, inum, &bi);
	if (ret)
		goto err;

	struct fuse_align_io align = align_io(c, bi.bi_size, 0);

	ret = -ENOMEM;
	buf = aligned_alloc(PAGE_SIZE, align.size);
	if (!buf)
		goto err;

	ret = read_aligned(c, inum, align.size, align.start, buf);
	if (ret)
		goto err;

	BUG_ON(buf[align.size - 1] != 0);

	fuse_reply_readlink(req, buf);

err:
	if (ret)
		fuse_reply_err(req, -ret);

	free(buf);
}

#if 0
/*
 * FUSE flush is essentially the close() call, however it is not guaranteed
 * that one flush happens per open/create.
 *
 * It doesn't have to do anything, and is mostly relevant for NFS-style
 * filesystems where close has some relationship to caching.
 */
static void bcachefs_fuse_flush(fuse_req_t req, fuse_ino_t inum,
				struct fuse_file_info *fi)
{
	struct bch_fs *c = fuse_req_userdata(req);
}

static void bcachefs_fuse_release(fuse_req_t req, fuse_ino_t inum,
				  struct fuse_file_info *fi)
{
	struct bch_fs *c = fuse_req_userdata(req);
}

static void bcachefs_fuse_fsync(fuse_req_t req, fuse_ino_t inum, int datasync,
				struct fuse_file_info *fi)
{
	struct bch_fs *c = fuse_req_userdata(req);
}

static void bcachefs_fuse_opendir(fuse_req_t req, fuse_ino_t inum,
				  struct fuse_file_info *fi)
{
	struct bch_fs *c = fuse_req_userdata(req);
}
#endif

struct fuse_dir_context {
	struct dir_context	ctx;
	fuse_req_t		req;
	char			*buf;
	size_t			bufsize;
};

struct fuse_dirent {
	uint64_t	ino;
	uint64_t	off;
	uint32_t	namelen;
	uint32_t	type;
	char name[];
};

#define FUSE_NAME_OFFSET offsetof(struct fuse_dirent, name)
#define FUSE_DIRENT_ALIGN(x) \
	(((x) + sizeof(uint64_t) - 1) & ~(sizeof(uint64_t) - 1))

static size_t fuse_add_direntry2(char *buf, size_t bufsize,
				 const char *name, int namelen,
				 const struct stat *stbuf, off_t off)
{
	size_t entlen		= FUSE_NAME_OFFSET + namelen;
	size_t entlen_padded	= FUSE_DIRENT_ALIGN(entlen);
	struct fuse_dirent *dirent = (struct fuse_dirent *) buf;

	if ((buf == NULL) || (entlen_padded > bufsize))
		return entlen_padded;

	dirent->ino = stbuf->st_ino;
	dirent->off = off;
	dirent->namelen = namelen;
	dirent->type = (stbuf->st_mode & S_IFMT) >> 12;
	memcpy(dirent->name, name, namelen);
	memset(dirent->name + namelen, 0, entlen_padded - entlen);

	return entlen_padded;
}

static int fuse_filldir(struct dir_context *_ctx,
			const char *name, int namelen,
			loff_t pos, u64 ino, unsigned type)
{
	struct fuse_dir_context *ctx =
		container_of(_ctx, struct fuse_dir_context, ctx);

	struct stat statbuf = {
		.st_ino		= unmap_root_ino(ino),
		.st_mode	= type << 12,
	};

	fuse_log(FUSE_LOG_DEBUG, "fuse_filldir(name=%s inum=%llu pos=%llu)\n",
		 name, statbuf.st_ino, pos);

	size_t len = fuse_add_direntry2(ctx->buf,
					ctx->bufsize,
					name,
					namelen,
					&statbuf,
					pos + 1);

	if (len > ctx->bufsize)
		return -1;

	ctx->buf	+= len;
	ctx->bufsize	-= len;

	return 0;
}

static bool handle_dots(struct fuse_dir_context *ctx, fuse_ino_t dir)
{
	if (ctx->ctx.pos == 0) {
		if (fuse_filldir(&ctx->ctx, ".", 1, ctx->ctx.pos,
				 dir, DT_DIR) < 0)
			return false;
		ctx->ctx.pos = 1;
	}

	if (ctx->ctx.pos == 1) {
		if (fuse_filldir(&ctx->ctx, "..", 2, ctx->ctx.pos,
				 /*TODO: parent*/ 1, DT_DIR) < 0)
			return false;
		ctx->ctx.pos = 2;
	}

	return true;
}

static void bcachefs_fuse_readdir(fuse_req_t req, fuse_ino_t dir_ino,
				  size_t size, off_t off,
				  struct fuse_file_info *fi)
{
	subvol_inum dir = map_root_ino(dir_ino);
	struct bch_fs *c = fuse_req_userdata(req);
	struct bch_inode_unpacked bi;
	char *buf = calloc(size, 1);
	struct fuse_dir_context ctx = {
		.ctx.actor	= fuse_filldir,
		.ctx.pos	= off,
		.req		= req,
		.buf		= buf,
		.bufsize	= size,
	};
	int ret = 0;

	fuse_log(FUSE_LOG_DEBUG, "bcachefs_fuse_readdir(dir=%llu, size=%zu, "
		 "off=%lld)\n", dir.inum, size, off);

	ret = bch2_inode_find_by_inum(c, dir, &bi);
	if (ret)
		goto reply;

	if (!S_ISDIR(bi.bi_mode)) {
		ret = -ENOTDIR;
		goto reply;
	}

	if (!handle_dots(&ctx, dir.inum))
		goto reply;

	ret = bch2_readdir(c, dir, &ctx.ctx);
reply:
	if (!ret) {
		fuse_log(FUSE_LOG_DEBUG, "bcachefs_fuse_readdir reply %zd\n",
					ctx.buf - buf);
		fuse_reply_buf(req, buf, ctx.buf - buf);
	} else {
		fuse_reply_err(req, -ret);
	}

	free(buf);
}

#if 0
static void bcachefs_fuse_readdirplus(fuse_req_t req, fuse_ino_t dir,
				      size_t size, off_t off,
				      struct fuse_file_info *fi)
{

}

static void bcachefs_fuse_releasedir(fuse_req_t req, fuse_ino_t inum,
				     struct fuse_file_info *fi)
{
	struct bch_fs *c = fuse_req_userdata(req);
}

static void bcachefs_fuse_fsyncdir(fuse_req_t req, fuse_ino_t inum, int datasync,
				   struct fuse_file_info *fi)
{
	struct bch_fs *c = fuse_req_userdata(req);
}
#endif

static void bcachefs_fuse_statfs(fuse_req_t req, fuse_ino_t inum)
{
	struct bch_fs *c = fuse_req_userdata(req);
	struct bch_fs_usage_short usage = bch2_fs_usage_read_short(c);
	unsigned shift = c->block_bits;
	struct statvfs statbuf = {
		.f_bsize	= block_bytes(c),
		.f_frsize	= block_bytes(c),
		.f_blocks	= usage.capacity >> shift,
		.f_bfree	= (usage.capacity - usage.used) >> shift,
		//.f_bavail	= statbuf.f_bfree,
		.f_files	= usage.nr_inodes,
		.f_ffree	= U64_MAX,
		.f_namemax	= BCH_NAME_MAX,
	};

	fuse_reply_statfs(req, &statbuf);
}

#if 0
static void bcachefs_fuse_setxattr(fuse_req_t req, fuse_ino_t inum,
				   const char *name, const char *value,
				   size_t size, int flags)
{
	struct bch_fs *c = fuse_req_userdata(req);
}

static void bcachefs_fuse_getxattr(fuse_req_t req, fuse_ino_t inum,
				   const char *name, size_t size)
{
	struct bch_fs *c = fuse_req_userdata(req);

	fuse_reply_xattr(req, );
}

static void bcachefs_fuse_listxattr(fuse_req_t req, fuse_ino_t inum, size_t size)
{
	struct bch_fs *c = fuse_req_userdata(req);
}

static void bcachefs_fuse_removexattr(fuse_req_t req, fuse_ino_t inum,
				      const char *name)
{
	struct bch_fs *c = fuse_req_userdata(req);
}
#endif

static void bcachefs_fuse_create(fuse_req_t req, fuse_ino_t dir_ino,
				 const char *name, mode_t mode,
				 struct fuse_file_info *fi)
{
	subvol_inum dir = map_root_ino(dir_ino);
	struct bch_fs *c = fuse_req_userdata(req);
	struct bch_inode_unpacked new_inode;
	int ret;

	fuse_log(FUSE_LOG_DEBUG, "bcachefs_fuse_create(%llu, %s, %x)\n",
		 dir.inum, name, mode);

	ret = do_create(c, dir, name, mode, 0, &new_inode);
	if (ret)
		goto err;

	struct fuse_entry_param e = inode_to_entry(c, &new_inode);
	fuse_reply_create(req, &e, fi);
	return;
err:
	fuse_reply_err(req, -ret);
}

#if 0
static void bcachefs_fuse_write_buf(fuse_req_t req, fuse_ino_t inum,
				    struct fuse_bufvec *bufv, off_t off,
				    struct fuse_file_info *fi)
{
	struct bch_fs *c = fuse_req_userdata(req);
}

static void bcachefs_fuse_fallocate(fuse_req_t req, fuse_ino_t inum, int mode,
				    off_t offset, off_t length,
				    struct fuse_file_info *fi)
{
	struct bch_fs *c = fuse_req_userdata(req);
}
#endif

static const struct fuse_lowlevel_ops bcachefs_fuse_ops = {
	.init		= bcachefs_fuse_init,
	.destroy	= bcachefs_fuse_destroy,
	.lookup		= bcachefs_fuse_lookup,
	.getattr	= bcachefs_fuse_getattr,
	.setattr	= bcachefs_fuse_setattr,
	.readlink	= bcachefs_fuse_readlink,
	.mknod		= bcachefs_fuse_mknod,
	.mkdir		= bcachefs_fuse_mkdir,
	.unlink		= bcachefs_fuse_unlink,
	.rmdir		= bcachefs_fuse_rmdir,
	.symlink	= bcachefs_fuse_symlink,
	.rename		= bcachefs_fuse_rename,
	.link		= bcachefs_fuse_link,
	.open		= bcachefs_fuse_open,
	.read		= bcachefs_fuse_read,
	.write		= bcachefs_fuse_write,
	//.flush	= bcachefs_fuse_flush,
	//.release	= bcachefs_fuse_release,
	//.fsync	= bcachefs_fuse_fsync,
	//.opendir	= bcachefs_fuse_opendir,
	.readdir	= bcachefs_fuse_readdir,
	//.readdirplus	= bcachefs_fuse_readdirplus,
	//.releasedir	= bcachefs_fuse_releasedir,
	//.fsyncdir	= bcachefs_fuse_fsyncdir,
	.statfs		= bcachefs_fuse_statfs,
	//.setxattr	= bcachefs_fuse_setxattr,
	//.getxattr	= bcachefs_fuse_getxattr,
	//.listxattr	= bcachefs_fuse_listxattr,
	//.removexattr	= bcachefs_fuse_removexattr,
	.create		= bcachefs_fuse_create,

	/* posix locks: */
#if 0
	.getlk		= bcachefs_fuse_getlk,
	.setlk		= bcachefs_fuse_setlk,
#endif
	//.write_buf	= bcachefs_fuse_write_buf,
	//.fallocate	= bcachefs_fuse_fallocate,

};

/*
 * Setup and command parsing.
 */

struct bf_context {
	char            *devices_str;
	char            **devices;
	int             nr_devices;
};

static void bf_context_free(struct bf_context *ctx)
{
	int i;

	free(ctx->devices_str);
	for (i = 0; i < ctx->nr_devices; ++i)
		free(ctx->devices[i]);
	free(ctx->devices);
}

static struct fuse_opt bf_opts[] = {
	FUSE_OPT_END
};

/*
 * Fuse option parsing helper -- returning 0 means we consumed the argument, 1
 * means we did not.
 */
static int bf_opt_proc(void *data, const char *arg, int key,
    struct fuse_args *outargs)
{
	struct bf_context *ctx = data;

	switch (key) {
	case FUSE_OPT_KEY_NONOPT:
		/* Just extract the first non-option string. */
		if (!ctx->devices_str) {
			ctx->devices_str = strdup(arg);
			return 0;
		}
		return 1;
	}

	return 1;
}

/*
 * dev1:dev2 -> [ dev1, dev2 ]
 * dev	     -> [ dev ]
 */
static void tokenize_devices(struct bf_context *ctx)
{
	char *devices_str = strdup(ctx->devices_str);
	char *devices_tmp = devices_str;
	char **devices = NULL;
	int nr = 0;
	char *dev = NULL;

	while ((dev = strsep(&devices_tmp, ":"))) {
		if (strlen(dev) > 0) {
			devices = realloc(devices, (nr + 1) * sizeof *devices);
			devices[nr] = strdup(dev);
			nr++;
		}
	}

	if (!devices) {
		devices = malloc(sizeof *devices);
		devices[0] = strdup(ctx->devices_str);
		nr = 1;
	}

	ctx->devices = devices;
	ctx->nr_devices = nr;

	free(devices_str);
}

static void usage(char *argv[])
{
	printf("Usage: %s fusemount [options] <dev>[:dev2:...] <mountpoint>\n",
	       argv[0]);
	printf("\n");
}

int cmd_fusemount(int argc, char *argv[])
{
	struct fuse_args args = FUSE_ARGS_INIT(argc, argv);
	struct bch_opts bch_opts = bch2_opts_empty();
	struct bf_context ctx = { 0 };
	struct bch_fs *c = NULL;
	int ret = 0, i;

	/* Parse arguments. */
	if (fuse_opt_parse(&args, &ctx, bf_opts, bf_opt_proc) < 0)
		die("fuse_opt_parse err: %m");

	struct fuse_cmdline_opts fuse_opts;
	if (fuse_parse_cmdline(&args, &fuse_opts) < 0)
		die("fuse_parse_cmdline err: %m");

	if (fuse_opts.show_help) {
		usage(argv);
		fuse_cmdline_help();
		fuse_lowlevel_help();
		ret = 0;
		goto out;
	}
	if (fuse_opts.show_version) {
		/* TODO: Show bcachefs version. */
		printf("FUSE library version %s\n", fuse_pkgversion());
		fuse_lowlevel_version();
		ret = 0;
		goto out;
	}
	if (!fuse_opts.mountpoint) {
		usage(argv);
		printf("Please supply a mountpoint.\n");
		ret = 1;
		goto out;
	}
	if (!ctx.devices_str) {
		usage(argv);
		printf("Please specify a device or device1:device2:...\n");
		ret = 1;
		goto out;
	}
	tokenize_devices(&ctx);

	struct printbuf fsname = PRINTBUF;
	prt_printf(&fsname, "fsname=");
	for (i = 0; i < ctx.nr_devices; ++i) {
		if (i)
			prt_str(&fsname, ":");
		prt_str(&fsname, ctx.devices[i]);
	}

	fuse_opt_add_arg(&args, "-o");
	fuse_opt_add_arg(&args, fsname.buf);

	/* Open bch */
	printf("Opening bcachefs filesystem on:\n");
	for (i = 0; i < ctx.nr_devices; ++i)
                printf("\t%s\n", ctx.devices[i]);

	c = bch2_fs_open(ctx.devices, ctx.nr_devices, bch_opts);
	if (IS_ERR(c))
		die("error opening %s: %s", ctx.devices_str,
		    bch2_err_str(PTR_ERR(c)));

	/* Fuse */
	struct fuse_session *se =
		fuse_session_new(&args, &bcachefs_fuse_ops,
				 sizeof(bcachefs_fuse_ops), c);
	if (!se)
		die("fuse_lowlevel_new err: %m");

	if (fuse_set_signal_handlers(se) < 0)
		die("fuse_set_signal_handlers err: %m");

	if (fuse_session_mount(se, fuse_opts.mountpoint))
		die("fuse_mount err: %m");

	/* This print statement is a trigger for tests. */
	printf("Fuse mount initialized.\n");

	if (fuse_opts.foreground == 0){
		printf("Fuse forcing to foreground mode, due gcc constructors usage.\n");
		fuse_opts.foreground = 1;
	}

	fuse_daemonize(fuse_opts.foreground);

	ret = fuse_session_loop(se);

	/* Cleanup */
	fuse_session_unmount(se);
	fuse_remove_signal_handlers(se);
	fuse_session_destroy(se);

out:
	free(fuse_opts.mountpoint);
	fuse_opt_free_args(&args);
	bf_context_free(&ctx);

	return ret ? 1 : 0;
}

#endif /* BCACHEFS_FUSE */
