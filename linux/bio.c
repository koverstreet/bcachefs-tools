/*
 * Copyright (C) 2001 Jens Axboe <axboe@kernel.dk>
 *
 * This program is free software; you can redistribute it and/or modify
 * it under the terms of the GNU General Public License version 2 as
 * published by the Free Software Foundation.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with this program; if not, write to the Free Software
 * Foundation, Inc., 59 Temple Place, Suite 330, Boston, MA  02111-
 *
 */
#include <linux/bio.h>
#include <linux/blkdev.h>
#include <linux/slab.h>
#include <linux/kernel.h>

static const struct {
	int		err;
	const char	*name;
} blk_errors[] = {
	[BLK_STS_OK]		= { 0,		"" },
	[BLK_STS_NOTSUPP]	= { -EOPNOTSUPP, "operation not supported" },
	[BLK_STS_TIMEOUT]	= { -ETIMEDOUT,	"timeout" },
	[BLK_STS_NOSPC]		= { -ENOSPC,	"critical space allocation" },
	[BLK_STS_TRANSPORT]	= { -ENOLINK,	"recoverable transport" },
	[BLK_STS_TARGET]	= { -EREMOTEIO,	"critical target" },
	[BLK_STS_NEXUS]		= { -EBADE,	"critical nexus" },
	[BLK_STS_MEDIUM]	= { -ENODATA,	"critical medium" },
	[BLK_STS_PROTECTION]	= { -EILSEQ,	"protection" },
	[BLK_STS_RESOURCE]	= { -ENOMEM,	"kernel resource" },
	[BLK_STS_AGAIN]		= { -EAGAIN,	"nonblocking retry" },

	/* device mapper special case, should not leak out: */
	[BLK_STS_DM_REQUEUE]	= { -EREMCHG, "dm internal retry" },

	/* everything else not covered above: */
	[BLK_STS_IOERR]		= { -EIO,	"I/O" },
};

int blk_status_to_errno(blk_status_t status)
{
	int idx = (__force int)status;

	if (WARN_ON_ONCE(idx >= ARRAY_SIZE(blk_errors)))
		return -EIO;
	return blk_errors[idx].err;
}

const char *blk_status_to_str(blk_status_t status)
{
	int idx = (__force int)status;

	if (WARN_ON_ONCE(idx >= ARRAY_SIZE(blk_errors)))
		return "(invalid error)";
	return blk_errors[idx].name;
}

void bio_copy_data_iter(struct bio *dst, struct bvec_iter *dst_iter,
			struct bio *src, struct bvec_iter *src_iter)
{
	struct bio_vec src_bv, dst_bv;
	void *src_p, *dst_p;
	unsigned bytes;

	while (src_iter->bi_size && dst_iter->bi_size) {
		src_bv = bio_iter_iovec(src, *src_iter);
		dst_bv = bio_iter_iovec(dst, *dst_iter);

		bytes = min(src_bv.bv_len, dst_bv.bv_len);

		src_p = kmap_atomic(src_bv.bv_page);
		dst_p = kmap_atomic(dst_bv.bv_page);

		memcpy(dst_p + dst_bv.bv_offset,
		       src_p + src_bv.bv_offset,
		       bytes);

		kunmap_atomic(dst_p);
		kunmap_atomic(src_p);

		flush_dcache_page(dst_bv.bv_page);

		bio_advance_iter(src, src_iter, bytes);
		bio_advance_iter(dst, dst_iter, bytes);
	}
}

/**
 * bio_copy_data - copy contents of data buffers from one bio to another
 * @src: source bio
 * @dst: destination bio
 *
 * Stops when it reaches the end of either @src or @dst - that is, copies
 * min(src->bi_size, dst->bi_size) bytes (or the equivalent for lists of bios).
 */
void bio_copy_data(struct bio *dst, struct bio *src)
{
	struct bvec_iter src_iter = src->bi_iter;
	struct bvec_iter dst_iter = dst->bi_iter;

	bio_copy_data_iter(dst, &dst_iter, src, &src_iter);
}

void zero_fill_bio_iter(struct bio *bio, struct bvec_iter start)
{
	unsigned long flags;
	struct bio_vec bv;
	struct bvec_iter iter;

	__bio_for_each_segment(bv, bio, iter, start) {
		char *data = bvec_kmap_irq(&bv, &flags);
		memset(data, 0, bv.bv_len);
		bvec_kunmap_irq(data, &flags);
	}
}

static int __bio_clone(struct bio *bio, struct bio *bio_src, gfp_t gfp)
{
	bio_set_flag(bio, BIO_CLONED);
	bio->bi_ioprio = bio_src->bi_ioprio;
	bio->bi_iter = bio_src->bi_iter;
	return 0;
}

struct bio *bio_alloc_clone(struct block_device *bdev, struct bio *bio_src,
		gfp_t gfp, struct bio_set *bs)
{
	struct bio *bio;

	bio = bio_alloc_bioset(bdev, 0, bio_src->bi_opf, gfp, bs);
	if (!bio)
		return NULL;

	if (__bio_clone(bio, bio_src, gfp) < 0) {
		bio_put(bio);
		return NULL;
	}
	bio->bi_io_vec = bio_src->bi_io_vec;

	return bio;
}

struct bio *bio_split(struct bio *bio, int sectors,
		      gfp_t gfp, struct bio_set *bs)
{
	struct bio *split = NULL;

	BUG_ON(sectors <= 0);
	BUG_ON(sectors >= bio_sectors(bio));

	split = bio_alloc_clone(bio->bi_bdev, bio, gfp, bs);
	if (!split)
		return NULL;

	split->bi_iter.bi_size = sectors << 9;

	bio_advance(bio, split->bi_iter.bi_size);

	return split;
}

void bio_free_pages(struct bio *bio)
{
	struct bvec_iter_all iter;
	struct bio_vec *bvec;

	bio_for_each_segment_all(bvec, bio, iter)
		__free_page(bvec->bv_page);
}

void bio_advance(struct bio *bio, unsigned bytes)
{
	bio_advance_iter(bio, &bio->bi_iter, bytes);
}

static void bio_free(struct bio *bio)
{
	struct bio_set *bs = bio->bi_pool;

	if (bs) {
		if (bio->bi_max_vecs > BIO_INLINE_VECS)
			mempool_free(bio->bi_io_vec, &bs->bvec_pool);

		mempool_free((void *) bio - bs->front_pad, &bs->bio_pool);
	} else {
		kfree(bio);
	}
}

void bio_put(struct bio *bio)
{
	if (!bio_flagged(bio, BIO_REFFED))
		bio_free(bio);
	else {
		BUG_ON(!atomic_read(&bio->__bi_cnt));

		/*
		 * last put frees it
		 */
		if (atomic_dec_and_test(&bio->__bi_cnt))
			bio_free(bio);
	}
}

int bio_add_page(struct bio *bio, struct page *page,
		 unsigned int len, unsigned int off)
{
	struct bio_vec *bv = &bio->bi_io_vec[bio->bi_vcnt];

	WARN_ON_ONCE(bio_flagged(bio, BIO_CLONED));
	WARN_ON_ONCE(bio->bi_vcnt >= bio->bi_max_vecs);

	bv->bv_page = page;
	bv->bv_offset = off;
	bv->bv_len = len;

	bio->bi_iter.bi_size += len;
	bio->bi_vcnt++;
	return len;
}

static inline bool bio_remaining_done(struct bio *bio)
{
	/*
	 * If we're not chaining, then ->__bi_remaining is always 1 and
	 * we always end io on the first invocation.
	 */
	if (!bio_flagged(bio, BIO_CHAIN))
		return true;

	BUG_ON(atomic_read(&bio->__bi_remaining) <= 0);

	if (atomic_dec_and_test(&bio->__bi_remaining)) {
		bio_clear_flag(bio, BIO_CHAIN);
		return true;
	}

	return false;
}

static struct bio *__bio_chain_endio(struct bio *bio)
{
	struct bio *parent = bio->bi_private;

	if (!parent->bi_status)
		parent->bi_status = bio->bi_status;
	bio_put(bio);
	return parent;
}

static void bio_chain_endio(struct bio *bio)
{
	bio_endio(__bio_chain_endio(bio));
}

void bio_endio(struct bio *bio)
{
again:
	if (!bio_remaining_done(bio))
		return;

	/*
	 * Need to have a real endio function for chained bios, otherwise
	 * various corner cases will break (like stacking block devices that
	 * save/restore bi_end_io) - however, we want to avoid unbounded
	 * recursion and blowing the stack. Tail call optimization would
	 * handle this, but compiling with frame pointers also disables
	 * gcc's sibling call optimization.
	 */
	if (bio->bi_end_io == bio_chain_endio) {
		bio = __bio_chain_endio(bio);
		goto again;
	}

	if (bio->bi_end_io)
		bio->bi_end_io(bio);
}

void bio_reset(struct bio *bio, struct block_device *bdev, unsigned int opf)
{
	unsigned long flags = bio->bi_flags & (~0UL << BIO_RESET_BITS);

	memset(bio, 0, BIO_RESET_BYTES);
	bio->bi_bdev	= bdev;
	bio->bi_opf	= opf;
	bio->bi_flags	= flags;
	atomic_set(&bio->__bi_remaining, 1);
}

struct bio *bio_kmalloc(unsigned int nr_iovecs, gfp_t gfp_mask)
{
	struct bio *bio;

	bio = kmalloc(sizeof(struct bio) +
		      sizeof(struct bio_vec) * nr_iovecs, gfp_mask);
	if (unlikely(!bio))
		return NULL;
	bio_init(bio, NULL, nr_iovecs ? bio->bi_inline_vecs : NULL, nr_iovecs, 0);
	bio->bi_pool = NULL;
	return bio;
}

static struct bio_vec *bvec_alloc(mempool_t *pool, int *nr_vecs,
		gfp_t gfp_mask)
{
	*nr_vecs = roundup_pow_of_two(*nr_vecs);
	/*
	 * Try a slab allocation first for all smaller allocations.  If that
	 * fails and __GFP_DIRECT_RECLAIM is set retry with the mempool.
	 * The mempool is sized to handle up to BIO_MAX_VECS entries.
	 */
	if (*nr_vecs < BIO_MAX_VECS) {
		struct bio_vec *bvl;

		bvl = kmalloc(sizeof(*bvl) * *nr_vecs, gfp_mask);
		if (likely(bvl))
			return bvl;
		*nr_vecs = BIO_MAX_VECS;
	}

	return mempool_alloc(pool, gfp_mask);
}

struct bio *bio_alloc_bioset(struct block_device *bdev,
			     unsigned nr_iovecs,
			     unsigned opf,
			     gfp_t gfp_mask,
			     struct bio_set *bs)
{
	struct bio *bio;
	void *p;

	if (nr_iovecs > BIO_MAX_VECS)
		return NULL;

	p = mempool_alloc(&bs->bio_pool, gfp_mask);
	if (unlikely(!p))
		return NULL;

	bio = p + bs->front_pad;
	if (nr_iovecs > BIO_INLINE_VECS) {
		struct bio_vec *bvl = NULL;

		bvl = bvec_alloc(&bs->bvec_pool, &nr_iovecs, gfp_mask);
		if (unlikely(!bvl))
			goto err_free;

		bio_init(bio, bdev, bvl, nr_iovecs, opf);
	} else if (nr_iovecs) {
		bio_init(bio, bdev, bio->bi_inline_vecs, BIO_INLINE_VECS, opf);
	} else {
		bio_init(bio, bdev, NULL, 0, opf);
	}

	bio->bi_pool = bs;
	return bio;

err_free:
	mempool_free(p, &bs->bio_pool);
	return NULL;
}

void bioset_exit(struct bio_set *bs)
{
	mempool_exit(&bs->bio_pool);
	mempool_exit(&bs->bvec_pool);
}

int bioset_init(struct bio_set *bs,
		unsigned int pool_size,
		unsigned int front_pad,
		int flags)
{
	int ret;

	bs->front_pad = front_pad;
	if (flags & BIOSET_NEED_BVECS)
		bs->back_pad = BIO_INLINE_VECS * sizeof(struct bio_vec);
	else
		bs->back_pad = 0;

	ret   = mempool_init_kmalloc_pool(&bs->bio_pool, pool_size, bs->front_pad +
					  sizeof(struct bio) + bs->back_pad) ?:
		mempool_init_kmalloc_pool(&bs->bvec_pool, pool_size,
					  sizeof(struct bio_vec) * BIO_MAX_VECS);
	if (ret)
		bioset_exit(bs);
	return ret;
}
