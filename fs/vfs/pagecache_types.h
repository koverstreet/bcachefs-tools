/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_VFS_PAGECACHE_TYPES_H
#define _BCACHEFS_VFS_PAGECACHE_TYPES_H

#ifndef NO_BCACHEFS_FS

#include "enum_kind.h"

typedef DARRAY(struct folio *) folios;

#define BCH_FOLIO_SECTOR_STATE()	\
	x(unallocated)			\
	x(reserved)			\
	x(dirty)			\
	x(dirty_reserved)		\
	x(allocated)

enum __enum_closed bch_folio_sector_state {
#define x(n)	SECTOR_##n,
	BCH_FOLIO_SECTOR_STATE()
#undef x
};

struct bch_folio_sector {
	/* Uncompressed, fully allocated replicas (or on disk reservation): */
	u8			nr_replicas:4,
	/* Owns PAGE_SECTORS * replicas_reserved sized in memory reservation: */
				replicas_reserved:4;
	u8			state;
};

struct bch_folio {
	spinlock_t		lock;
	atomic_t		write_count;
	/* is s[] up to date with the btree? says nothing about the data */
	bool			state_uptodate;
	/* the count s[].replicas_reserved is charged at, and released at */
	u8			replicas_reserved_at;
	/*
	 * A foreground reservation fell back: writeback shouldn't insist on the
	 * inode's count - see bch2_get_folio_disk_reservation().
	 */
	bool			reserved_degraded;
	/*
	 * The data: sectors [0, partially_uptodate) are read but the folio
	 * isn't uptodate. One offset suffices because reads start at the front
	 * of the folio; 0 means nothing partial, so only
	 * readpage_bio_drop_unissued() maintains this.
	 */
	u16			partially_uptodate;
	struct bch_folio_sector	s[];
};

struct bch2_folio_reservation {
	struct disk_reservation	disk;
	struct quota_res	quota;
	/* @disk fell back to fewer replicas than the inode asks for */
	bool			degraded;
};

#endif

#endif /* _BCACHEFS_VFS_PAGECACHE_TYPES_H */
