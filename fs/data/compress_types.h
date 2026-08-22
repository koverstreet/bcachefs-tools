/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_DATA_COMPRESS_TYPES_H
#define _BCACHEFS_DATA_COMPRESS_TYPES_H

/*
 * Compressing and decompressing want wildly different amounts of scratch
 * space, so they get separate pools: a zstd compression context carries the
 * match finder's hash and chain tables, sized by the compression level, while
 * decompression carries only the huffman and FSE decode tables - it doesn't
 * search, it follows offsets. One pool sized to the larger would make every
 * read pay the write side's footprint.
 *
 * Both are indexed by compression type rather than shared, because a type can
 * be enabled after mount (bch2_check_set_has_compressed_data()) and a mempool
 * cannot grow.
 */
struct bch_fs_compress {
	mempool_t		bounce[2];
	mempool_t		workspace[BCH_COMPRESSION_OPT_NR];
	mempool_t		decompress_workspace[BCH_COMPRESSION_OPT_NR];
	size_t			zstd_workspace_size;
};

#endif /* _BCACHEFS_DATA_COMPRESS_TYPES_H */
