/*
 * 2.5 block I/O model
 *
 * Copyright (C) 2001 Jens Axboe <axboe@suse.de>
 *
 * This program is free software; you can redistribute it and/or modify
 * it under the terms of the GNU General Public License version 2 as
 * published by the Free Software Foundation.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 *
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with this program; if not, write to the Free Software
 * Foundation, Inc., 59 Temple Place, Suite 330, Boston, MA  02111-
 */
#ifndef __LINUX_BIO_H
#define __LINUX_BIO_H

#include <linux/mempool.h>
#include <linux/bug.h>
#include <linux/err.h>

#include <linux/blkdev.h>
#include <linux/blk_types.h>
#include <linux/workqueue.h>

#define bio_prio(bio)			(bio)->bi_ioprio
#define bio_set_prio(bio, prio)		((bio)->bi_ioprio = prio)

#define bio_iter_iovec(bio, iter)				\
	bvec_iter_bvec((bio)->bi_io_vec, (iter))

#define bio_iter_page(bio, iter)				\
	bvec_iter_page((bio)->bi_io_vec, (iter))
#define bio_iter_len(bio, iter)					\
	bvec_iter_len((bio)->bi_io_vec, (iter))
#define bio_iter_offset(bio, iter)				\
	bvec_iter_offset((bio)->bi_io_vec, (iter))

#define bio_page(bio)		bio_iter_page((bio), (bio)->bi_iter)
#define bio_offset(bio)		bio_iter_offset((bio), (bio)->bi_iter)
#define bio_iovec(bio)		bio_iter_iovec((bio), (bio)->bi_iter)

#define bio_multiple_segments(bio)				\
	((bio)->bi_iter.bi_size != bio_iovec(bio).bv_len)

#define bvec_iter_sectors(iter)	((iter).bi_size >> 9)
#define bvec_iter_end_sector(iter) ((iter).bi_sector + bvec_iter_sectors((iter)))

#define bio_sectors(bio)	bvec_iter_sectors((bio)->bi_iter)
#define bio_end_sector(bio)	bvec_iter_end_sector((bio)->bi_iter)

static inline bool bio_has_data(struct bio *bio)
{
	if (bio &&
	    bio->bi_iter.bi_size &&
	    bio_op(bio) != REQ_OP_DISCARD &&
	    bio_op(bio) != REQ_OP_SECURE_ERASE)
		return true;

	return false;
}

static inline bool bio_no_advance_iter(struct bio *bio)
{
	return bio_op(bio) == REQ_OP_DISCARD ||
	       bio_op(bio) == REQ_OP_SECURE_ERASE ||
	       bio_op(bio) == REQ_OP_WRITE_SAME;
}

static inline bool bio_is_rw(struct bio *bio)
{
	if (!bio_has_data(bio))
		return false;

	if (bio_no_advance_iter(bio))
		return false;

	return true;
}

static inline bool bio_mergeable(struct bio *bio)
{
	if (bio->bi_opf & REQ_NOMERGE_FLAGS)
		return false;

	return true;
}

static inline unsigned int bio_cur_bytes(struct bio *bio)
{
	if (bio_has_data(bio))
		return bio_iovec(bio).bv_len;
	else /* dataless requests such as discard */
		return bio->bi_iter.bi_size;
}

static inline void *bio_data(struct bio *bio)
{
	if (bio_has_data(bio))
		return page_address(bio_page(bio)) + bio_offset(bio);

	return NULL;
}

#define __bio_kmap_atomic(bio, iter)				\
	(kmap_atomic(bio_iter_iovec((bio), (iter)).bv_page) +	\
		bio_iter_iovec((bio), (iter)).bv_offset)

#define __bio_kunmap_atomic(addr)	kunmap_atomic(addr)

static inline struct bio_vec *bio_next_segment(const struct bio *bio,
					       struct bvec_iter_all *iter)
{
	if (iter->idx >= bio->bi_vcnt)
		return NULL;

	return &bio->bi_io_vec[iter->idx];
}

#define bio_for_each_segment_all(bvl, bio, iter) \
	for ((iter).idx = 0; (bvl = bio_next_segment((bio), &(iter))); (iter).idx++)

static inline void bio_advance_iter(struct bio *bio, struct bvec_iter *iter,
				    unsigned bytes)
{
	iter->bi_sector += bytes >> 9;

	if (bio_no_advance_iter(bio))
		iter->bi_size -= bytes;
	else
		bvec_iter_advance(bio->bi_io_vec, iter, bytes);
}

#define __bio_for_each_segment(bvl, bio, iter, start)			\
	for (iter = (start);						\
	     (iter).bi_size &&						\
		((bvl = bio_iter_iovec((bio), (iter))), 1);		\
	     bio_advance_iter((bio), &(iter), (bvl).bv_len))

#define bio_for_each_segment(bvl, bio, iter)				\
	__bio_for_each_segment(bvl, bio, iter, (bio)->bi_iter)

#define __bio_for_each_bvec(bvl, bio, iter, start)			\
	__bio_for_each_segment(bvl, bio, iter, start)

#define bio_iter_last(bvec, iter) ((iter).bi_size == (bvec).bv_len)

static inline unsigned bio_segments(struct bio *bio)
{
	unsigned segs = 0;
	struct bio_vec bv;
	struct bvec_iter iter;

	/*
	 * We special case discard/write same, because they interpret bi_size
	 * differently:
	 */

	if (bio_op(bio) == REQ_OP_DISCARD)
		return 1;

	if (bio_op(bio) == REQ_OP_SECURE_ERASE)
		return 1;

	if (bio_op(bio) == REQ_OP_WRITE_SAME)
		return 1;

	bio_for_each_segment(bv, bio, iter)
		segs++;

	return segs;
}

static inline void bio_get(struct bio *bio)
{
	bio->bi_flags |= (1 << BIO_REFFED);
	smp_mb__before_atomic();
	atomic_inc(&bio->__bi_cnt);
}

static inline bool bio_flagged(struct bio *bio, unsigned int bit)
{
	return (bio->bi_flags & (1U << bit)) != 0;
}

static inline void bio_set_flag(struct bio *bio, unsigned int bit)
{
	bio->bi_flags |= (1U << bit);
}

static inline void bio_clear_flag(struct bio *bio, unsigned int bit)
{
	bio->bi_flags &= ~(1U << bit);
}

extern struct bio *bio_split(struct bio *bio, int sectors,
			     gfp_t gfp, struct bio_set *bs);

static inline struct bio *bio_next_split(struct bio *bio, int sectors,
					 gfp_t gfp, struct bio_set *bs)
{
	if (sectors >= bio_sectors(bio))
		return bio;

	return bio_split(bio, sectors, gfp, bs);
}

struct bio_set {
	unsigned int front_pad;
	unsigned int back_pad;
	mempool_t bio_pool;
	mempool_t bvec_pool;
};


static inline void bioset_free(struct bio_set *bs)
{
	kfree(bs);
}

void bioset_exit(struct bio_set *);
int bioset_init(struct bio_set *, unsigned, unsigned, int);

extern struct bio_set *bioset_create(unsigned int, unsigned int);
extern struct bio_set *bioset_create_nobvec(unsigned int, unsigned int);
enum {
	BIOSET_NEED_BVECS	= 1 << 0,
	BIOSET_NEED_RESCUER	= 1 << 1,
};

struct bio *bio_alloc_bioset(struct block_device *, unsigned,
			     unsigned, gfp_t, struct bio_set *);
extern void bio_put(struct bio *);

int bio_add_page(struct bio *, struct page *, unsigned, unsigned);

struct bio *bio_alloc_clone(struct block_device *, struct bio *,
			    gfp_t, struct bio_set *);

struct bio *bio_kmalloc(unsigned int, gfp_t);

extern void bio_endio(struct bio *);

extern void bio_advance(struct bio *, unsigned);

extern void bio_reset(struct bio *, struct block_device *, unsigned);
void bio_chain(struct bio *, struct bio *);

extern void bio_copy_data_iter(struct bio *dst, struct bvec_iter *dst_iter,
			       struct bio *src, struct bvec_iter *src_iter);
extern void bio_copy_data(struct bio *dst, struct bio *src);

void bio_free_pages(struct bio *bio);

void zero_fill_bio_iter(struct bio *bio, struct bvec_iter iter);

static inline void zero_fill_bio(struct bio *bio)
{
	zero_fill_bio_iter(bio, bio->bi_iter);
}

#define bio_set_dev(bio, bdev)			\
do {						\
	(bio)->bi_bdev = (bdev);		\
} while (0)

#define bio_copy_dev(dst, src)			\
do {						\
	(dst)->bi_bdev = (src)->bi_bdev;	\
} while (0)

static inline char *bvec_kmap_irq(struct bio_vec *bvec, unsigned long *flags)
{
	return page_address(bvec->bv_page) + bvec->bv_offset;
}

static inline void bvec_kunmap_irq(char *buffer, unsigned long *flags)
{
	*flags = 0;
}

static inline char *__bio_kmap_irq(struct bio *bio, struct bvec_iter iter,
				   unsigned long *flags)
{
	return bvec_kmap_irq(&bio_iter_iovec(bio, iter), flags);
}
#define __bio_kunmap_irq(buf, flags)	bvec_kunmap_irq(buf, flags)

#define bio_kmap_irq(bio, flags) \
	__bio_kmap_irq((bio), (bio)->bi_iter, (flags))
#define bio_kunmap_irq(buf,flags)	__bio_kunmap_irq(buf, flags)

struct bio_list {
	struct bio *head;
	struct bio *tail;
};

static inline int bio_list_empty(const struct bio_list *bl)
{
	return bl->head == NULL;
}

static inline void bio_list_init(struct bio_list *bl)
{
	bl->head = bl->tail = NULL;
}

#define BIO_EMPTY_LIST	{ NULL, NULL }

#define bio_list_for_each(bio, bl) \
	for (bio = (bl)->head; bio; bio = bio->bi_next)

static inline unsigned bio_list_size(const struct bio_list *bl)
{
	unsigned sz = 0;
	struct bio *bio;

	bio_list_for_each(bio, bl)
		sz++;

	return sz;
}

static inline void bio_list_add(struct bio_list *bl, struct bio *bio)
{
	bio->bi_next = NULL;

	if (bl->tail)
		bl->tail->bi_next = bio;
	else
		bl->head = bio;

	bl->tail = bio;
}

static inline void bio_list_add_head(struct bio_list *bl, struct bio *bio)
{
	bio->bi_next = bl->head;

	bl->head = bio;

	if (!bl->tail)
		bl->tail = bio;
}

static inline void bio_list_merge(struct bio_list *bl, struct bio_list *bl2)
{
	if (!bl2->head)
		return;

	if (bl->tail)
		bl->tail->bi_next = bl2->head;
	else
		bl->head = bl2->head;

	bl->tail = bl2->tail;
}

static inline void bio_list_merge_head(struct bio_list *bl,
				       struct bio_list *bl2)
{
	if (!bl2->head)
		return;

	if (bl->head)
		bl2->tail->bi_next = bl->head;
	else
		bl->tail = bl2->tail;

	bl->head = bl2->head;
}

static inline struct bio *bio_list_peek(struct bio_list *bl)
{
	return bl->head;
}

static inline struct bio *bio_list_pop(struct bio_list *bl)
{
	struct bio *bio = bl->head;

	if (bio) {
		bl->head = bl->head->bi_next;
		if (!bl->head)
			bl->tail = NULL;

		bio->bi_next = NULL;
	}

	return bio;
}

static inline struct bio *bio_list_get(struct bio_list *bl)
{
	struct bio *bio = bl->head;

	bl->head = bl->tail = NULL;

	return bio;
}

/*
 * Increment chain count for the bio. Make sure the CHAIN flag update
 * is visible before the raised count.
 */
static inline void bio_inc_remaining(struct bio *bio)
{
	bio_set_flag(bio, BIO_CHAIN);
	smp_mb__before_atomic();
	atomic_inc(&bio->__bi_remaining);
}

static inline void bio_init(struct bio *bio,
			    struct block_device *bdev,
			    struct bio_vec *table,
			    unsigned short max_vecs,
			    unsigned int opf)
{
	memset(bio, 0, sizeof(*bio));
	bio->bi_bdev = bdev;
	bio->bi_opf = opf;
	atomic_set(&bio->__bi_remaining, 1);
	atomic_set(&bio->__bi_cnt, 1);

	bio->bi_io_vec = table;
	bio->bi_max_vecs = max_vecs;
}

#endif /* __LINUX_BIO_H */
