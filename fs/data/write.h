/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_IO_WRITE_H
#define _BCACHEFS_IO_WRITE_H

#include "data/checksum.h"
#include "data/write_gen.h"
#include "util/bio/pages_gen.h"

#define to_wbio(_bio)			\
	container_of((_bio), struct bch_write_bio, bio)

static inline struct bch_write_bio *wbio_init(struct bio *bio)
{
	struct bch_write_bio *wbio = to_wbio(bio);

	memset(&wbio->wbio, 0, sizeof(wbio->wbio));
	return wbio;
}

#endif /* _BCACHEFS_IO_WRITE_H */
