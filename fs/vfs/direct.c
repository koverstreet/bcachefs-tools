// SPDX-License-Identifier: GPL-2.0
#ifndef NO_BCACHEFS_FS

#include "bcachefs.h"

#include "data/read.h"

#include "vfs/fs.h"
#include "vfs/io.h"
#include "vfs/direct.h"
#include "vfs/pagecache.h"

#include "vendor/bio_iov_iter.h"

#include <linux/pagemap.h>

/* O_DIRECT reads */

static void bio_check_or_release(struct bio *bio, bool check_dirty)
{
	if (check_dirty) {
		bch2_bio_check_pages_dirty(bio);
	} else {
		bio_release_pages(bio, false);
		bio_put(bio);
	}
}

static CLOSURE_CALLBACK(bch2_dio_read_complete)
{
	closure_type(dio, struct dio_read, cl);

	dio->req->ki_complete(dio->req, dio->ret);
	bio_check_or_release(&dio->rbio.bio, dio->should_dirty);
}

static void bch2_direct_IO_read_endio(struct bio *bio)
{
	struct bch_read_bio *rbio = to_rbio(bio);
	struct dio_read *dio = bio->bi_private;

	/*
	 * no_poison_check asks for what is physically there. The read still
	 * failed, and said so - err_report carries which errors - but the bytes
	 * it got are in the caller's buffer, and replacing the count with the
	 * error is what throws them away.
	 */
	if (rbio->ret && !(dio->flags & BCH_READ_no_poison_check))
		dio->ret = bch2_err_class(rbio->ret);

	closure_put(&dio->cl);
}

static void bch2_direct_IO_read_split_endio(struct bio *bio)
{
	struct dio_read *dio = bio->bi_private;
	bool should_dirty = dio->should_dirty;

	bch2_direct_IO_read_endio(bio);
	bio_check_or_release(bio, should_dirty);
}

static int __bch2_direct_IO_read(struct kiocb *req, struct iov_iter *iter,
				enum bch_read_flags flags,
				struct bch_read_err_report *err_report)
{
	struct file *file = req->ki_filp;
	struct bch_inode_info *inode = file_bch_inode(file);
	struct bch_fs *c = inode->v.i_sb->s_fs_info;
	struct dio_read *dio;
	struct bio *bio;
	loff_t offset = req->ki_pos;
	bool sync = is_sync_kiocb(req);
	bool split = false;
	size_t shorten;
	ssize_t ret;

	struct bch_inode_opts opts;
	bch2_inode_opts_get_inode(c, &inode->ei_inode, &opts);

	/* bios must be 512 byte aligned: */
	if ((offset|iter->count) & (SECTOR_SIZE - 1))
		return bch_err_throw(c, EINVAL_unaligned_io);

	ret = min_t(loff_t, iter->count,
		    max_t(loff_t, 0, i_size_read(&inode->v) - offset));

	if (!ret)
		return ret;

	shorten = iov_iter_count(iter) - round_up(ret, block_bytes(c));
	if (shorten >= iter->count)
		shorten = 0;
	iter->count -= shorten;

	bio = bio_alloc_bioset(NULL,
			       bio_iov_vecs_to_alloc(iter, BIO_MAX_VECS),
			       REQ_OP_READ,
			       GFP_KERNEL,
			       &c->vfs.dio_read_bioset);

	dio = container_of(bio, struct dio_read, rbio.bio);
	closure_init(&dio->cl, NULL);

	/*
	 * this is a _really_ horrible hack just to avoid an atomic sub at the
	 * end:
	 */
	if (!sync) {
		set_closure_fn(&dio->cl, bch2_dio_read_complete, NULL);
		atomic_set(&dio->cl.remaining,
			   CLOSURE_REMAINING_INITIALIZER -
			   CLOSURE_RUNNING +
			   CLOSURE_DESTRUCTOR);
	} else {
		atomic_set(&dio->cl.remaining,
			   CLOSURE_REMAINING_INITIALIZER + 1);
	}

	dio->req	= req;
	dio->ret	= ret;
	dio->flags	= flags;
	/*
	 * This is one of the sketchier things I've encountered: we have to skip
	 * the dirtying of requests that are internal from the kernel (i.e. from
	 * loopback), because we'll deadlock on page_lock.
	 */
	dio->should_dirty = user_backed_iter(iter);

	goto start;
	while (iter->count) {
		split = true;

		bio = bio_alloc_bioset(NULL,
				       bio_iov_vecs_to_alloc(iter, BIO_MAX_VECS),
				       REQ_OP_READ,
				       GFP_KERNEL,
				       &c->bio_read);
start:
		struct bch_read_bio *rbio =
			rbio_init(bio,
				  c,
				  opts,
				  split
				  ? bch2_direct_IO_read_split_endio
				  : bch2_direct_IO_read_endio);

		BUG_ON(rbio->_state);
		rbio->err_report		= err_report;
		rbio->subvol			= inode_inum(inode).subvol;
		rbio->bio.bi_opf		= REQ_OP_READ|REQ_SYNC;
		rbio->bio.bi_iter.bi_sector	= offset >> 9;
		rbio->bio.bi_private		= dio;

		ret = bch2_bio_iov_iter_get_pages(&rbio->bio, iter, 0);
		if (ret < 0) {
			/* XXX: fault inject this path */
			rbio->ret = ret;
			bio_endio(&rbio->bio);
			break;
		}

		offset += rbio->bio.bi_iter.bi_size;

		if (dio->should_dirty)
			bch2_bio_set_pages_dirty(&rbio->bio);

		if (iter->count)
			closure_get(&dio->cl);

		CLASS(btree_trans, trans)(c);
		bch2_read(trans, rbio, rbio->bio.bi_iter, inode_inum(inode),
			  NULL, NULL,
			  BCH_READ_retry_if_stale|
			  BCH_READ_may_promote|
			  BCH_READ_user_mapped|
			  flags);
	}

	iter->count += shorten;

	if (sync) {
		closure_sync(&dio->cl);
		closure_debug_destroy(&dio->cl);
		ret = dio->ret;
		bio_check_or_release(&dio->rbio.bio, dio->should_dirty);
		return ret;
	} else {
		return -EIOCBQUEUED;
	}
}

int bch2_direct_IO_read(struct kiocb *req, struct iov_iter *iter,
			enum bch_read_flags flags,
			struct bch_read_err_report *err_report)
{
	struct file *file = req->ki_filp;
	struct address_space *mapping = file->f_mapping;

	if (unlikely(mapping->nrpages)) {
		ssize_t ret = filemap_write_and_wait_range(mapping,
					req->ki_pos,
					req->ki_pos + iov_iter_count(iter) - 1);
		if (ret < 0)
			return ret;
	}

	file_accessed(file);

	struct blk_plug plug;
	blk_start_plug(&plug);
	ssize_t ret = __bch2_direct_IO_read(req, iter, flags, err_report);
	blk_finish_plug(&plug);

	if (ret >= 0)
		req->ki_pos += ret;
	return ret;
}

ssize_t bch2_read_iter(struct kiocb *iocb, struct iov_iter *iter)
{
	struct file *file = iocb->ki_filp;
	ssize_t ret = 0;

	if (!iov_iter_count(iter))
		return 0; /* skip atime */

	if (iocb->ki_flags & IOCB_DIRECT) {
		ret = bch2_direct_IO_read(iocb, iter, 0, NULL);
	} else {
		guard(bch2_pagecache_add)(file_bch_inode(file));
		ret = filemap_read(iocb, iter, ret);
	}

	return bch2_err_class(ret);
}

#endif /* NO_BCACHEFS_FS */
