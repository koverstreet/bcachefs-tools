/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_FS_IO_H
#define _BCACHEFS_FS_IO_H

#ifndef NO_BCACHEFS_FS

#include "alloc/buckets.h"
#include "data/write_gen.h"
#include "fs/inode.h"
#include "fs/quota_gen.h"
#include <linux/hash.h>
#include "vendor/closure.h"
#include "vfs/fdm_gen.h"
#include "vfs/fs.h"

#include <linux/uio.h>

#include "vfs/io_gen.h"

static inline struct folio_vec biovec_to_foliovec(struct bio_vec bv)
{

	struct folio *folio	= page_folio(bv.bv_page);
	size_t offset		= (folio_page_idx(folio, bv.bv_page) << PAGE_SHIFT) +
		bv.bv_offset;
	size_t len = min_t(size_t, folio_size(folio) - offset, bv.bv_len);

	return (struct folio_vec) {
		.fv_folio	= folio,
		.fv_offset	= offset,
		.fv_len		= len,
	};
}

static inline struct folio_vec bio_iter_iovec_folio(struct bio *bio,
						    struct bvec_iter iter)
{
	return biovec_to_foliovec(bio_iter_iovec(bio, iter));
}

#define __bio_for_each_folio(bvl, bio, iter, start)			\
	for (iter = (start);						\
	     (iter).bi_size &&						\
		((bvl = bio_iter_iovec_folio((bio), (iter))), 1);	\
	     bio_advance_iter_single((bio), &(iter), (bvl).fv_len))

/**
 * bio_for_each_folio - iterate over folios within a bio
 *
 * Like other non-_all versions, this iterates over what bio->bi_iter currently
 * points to. This version is for drivers, where the bio may have previously
 * been split or cloned.
 */
#define bio_for_each_folio(bvl, bio, iter)				\
	__bio_for_each_folio(bvl, bio, iter, (bio)->bi_iter)

#endif

#endif /* _BCACHEFS_FS_IO_H */
