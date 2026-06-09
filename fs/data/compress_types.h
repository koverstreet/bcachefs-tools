/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_DATA_COMPRESS_TYPES_H
#define _BCACHEFS_DATA_COMPRESS_TYPES_H

struct bch_fs_compress {
	mempool_t		bounce[2];
	mempool_t		workspace[BCH_COMPRESSION_OPT_NR];
	/*
	 * Mount-immutable after first __bch2_fs_compress_init() call.
	 * Derived from zstd_max_clevel() and c->opts.encoded_extent_max,
	 * both of which are fixed at mount time.  Read lock-free on the
	 * compression hot path; init writer pairs with WRITE_ONCE, readers
	 * use READ_ONCE.  Zero means "not yet initialised".
	 */
	size_t			zstd_workspace_size;
};

#endif /* _BCACHEFS_DATA_COMPRESS_TYPES_H */
