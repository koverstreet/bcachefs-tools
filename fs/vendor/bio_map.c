// SPDX-License-Identifier: GPL-2.0

#include <linux/bio.h>
#include <linux/blkdev.h>
#include <linux/mm.h>
#include <linux/slab.h>

#include "vendor/bio_map.h"

/* A page each for vmalloc memory, one for the direct map */
static unsigned buf_nr_bvecs(void *base, size_t size)
{
	return is_vmalloc_addr(base)
		? DIV_ROUND_UP(((unsigned long) base & (PAGE_SIZE - 1)) + size, PAGE_SIZE)
		: 1;
}

static void bio_map(struct bio *bio, void *base, size_t size)
{
	if (is_vmalloc_addr(base))
		BUG_ON(!bio_add_vmalloc(bio, base, size));
	else
		bio_add_virt_nofail(bio, base, size);
}

/*
 * Allocate a bio (possibly a chain of bios via bio_chain()) covering
 * [base, base+size) on @bdev starting at sector @sector. Each bio in the
 * chain is capped at BIO_MAX_VECS bvecs; physically contiguous buffers fit
 * in one bvec and the chain typically has length 1.
 *
 * The returned bio is the tail of the chain (the last chunk in sector
 * order), left unsubmitted for the caller to configure (bi_end_io,
 * bi_private, any wrapper state) and submit. Earlier chunks are chained
 * to the bio that immediately follows them and submitted here in sector
 * order, so the block layer sees requests in forward order; callers
 * should wrap their submission in blk_start_plug()/blk_finish_plug() to
 * give the driver a chance to merge.
 *
 * When the caller's tail bio completes, the chain's bi_status reflects
 * any error propagated from the earlier chunks.
 *
 * REQ_PREFLUSH, if set in @opf, applies only to the first (sector-lowest)
 * bio in the chain.
 */
struct bio *bch2_bio_map_and_chain(struct block_device *bdev,
				   void *base, size_t size,
				   sector_t sector,
				   blk_opf_t opf, gfp_t gfp,
				   struct bio_set *bs)
{
	struct bio *tail	= NULL;
	unsigned nr_bvecs	= buf_nr_bvecs(base, size);

	while (size) {
		size_t chunk_size = size;
		unsigned chunk_bvecs = nr_bvecs;

		if (chunk_bvecs > BIO_MAX_VECS) {
			unsigned offset = (unsigned long) base & (PAGE_SIZE - 1);

			chunk_size = (size_t)BIO_MAX_VECS * PAGE_SIZE - offset;
			chunk_bvecs = BIO_MAX_VECS;
			BUG_ON(chunk_size > size);
		}

		struct bio *bio = bio_alloc_bioset(bdev, chunk_bvecs,
						   opf, gfp, bs);
		opf &= ~REQ_PREFLUSH; /* only on the first bio */

		bio_map(bio, base, chunk_size);
		bio->bi_iter.bi_sector = sector;

		if (tail) {
			/*
			 * Chain the previous (earlier-sector) bio to this one
			 * and submit it now: submission order is sector-forward
			 * so the elevator can merge with adjacent writes.
			 */
			bio_chain(tail, bio);
			submit_bio(tail);
		}
		tail = bio;

		base		+= chunk_size;
		sector		+= chunk_size >> 9;
		size		-= chunk_size;
		nr_bvecs	-= chunk_bvecs;
	}

	return tail;
}
