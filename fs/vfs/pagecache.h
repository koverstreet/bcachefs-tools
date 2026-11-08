/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_FS_IO_PAGECACHE_H
#define _BCACHEFS_FS_IO_PAGECACHE_H

#include "vfs/io.h"

#include <linux/pagemap.h>

#include "vfs/pagecache_gen.h"
#include "vfs/folio_reservation_gen.h"

/*
 * Use u64 for the end pos and sector helpers because if the folio covers the
 * max supported range of the mapping, the start offset of the next folio
 * overflows loff_t. This breaks much of the range based processing in the
 * buffered write path.
 */
static inline u64 folio_end_pos(struct folio *folio)
{
	return folio_pos(folio) + folio_size(folio);
}

static inline loff_t folio_sector(struct folio *folio)
{
	return folio_pos(folio) >> 9;
}

static inline u64 folio_end_sector(struct folio *folio)
{
	return folio_end_pos(folio) >> 9;
}

/* for newly allocated folios: */
static inline void __bch2_folio_release(struct folio *folio)
{
	kfree(folio_detach_private(folio));
}

static inline void bch2_folio_release(struct folio *folio)
{
	EBUG_ON(!folio_test_locked(folio));
	__bch2_folio_release(folio);
}

static inline struct bch_folio *bch2_folio(struct folio *folio)
{
	return folio_get_private(folio);
}

#endif /* _BCACHEFS_FS_IO_PAGECACHE_H */
