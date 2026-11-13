/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_VENDOR_BIO_MAP_H
#define _BCACHEFS_VENDOR_BIO_MAP_H

#include <linux/blk_types.h>

struct bio *bch2_bio_map_and_chain(struct block_device *, void *, size_t,
				   sector_t, blk_opf_t, gfp_t,
				   struct bio_set *);

#endif /* _BCACHEFS_VENDOR_BIO_MAP_H */
