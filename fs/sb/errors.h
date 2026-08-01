/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_SB_ERRORS_H
#define _BCACHEFS_SB_ERRORS_H

#include "sb/errors_types.h"

extern const char * const bch2_sb_error_strs[];

void bch2_sb_error_id_to_text(struct printbuf *, enum bch_sb_error_id);
void bch2_prt_error_nr(struct printbuf *, u64);
void bch2_fs_errors_to_text(struct printbuf *, struct bch_fs *);

extern const struct bch_sb_field_ops bch_sb_field_ops_errors;
extern const struct bch_sb_field_ops bch_sb_field_ops_errors_v2;

void bch2_sb_error_count(struct bch_fs *, enum bch_sb_error_id);

/*
 * Block statuses we count separately: what the device said is the first
 * thing worth knowing from a filesystem that's logging IO errors - media
 * errors mean the drive is going, timeouts mean it's hanging, transport
 * errors point at the fabric, and INVAL means the request itself was
 * rejected: not an error the device hit, an error in what we handed it.
 *
 * Curated by hand rather than generated from BLK_ERRS(): sb error ids are
 * permanent on-disk numbers, so we don't spend them on statuses that can't
 * reach a bcachefs completion - DM_REQUEUE is consumed inside dm, and the
 * zone codes want zoned device support we don't have. Anything left out
 * counts as blk_sts_unknown; to promote one, add it here and add a
 * numbered entry in errors_format.h - the build breaks if you forget.
 */
#define BCH_BLK_STS_SB_ERRS()			\
	x(NOTSUPP,		notsupp)	\
	x(TIMEOUT,		timeout)	\
	x(NOSPC,		nospc)		\
	x(TRANSPORT,		transport)	\
	x(TARGET,		target)		\
	x(RESV_CONFLICT,	resv_conflict)	\
	x(MEDIUM,		medium)		\
	x(PROTECTION,		protection)	\
	x(RESOURCE,		resource)	\
	x(IOERR,		ioerr)		\
	x(AGAIN,		again)		\
	x(DEV_RESOURCE,		dev_resource)	\
	x(OFFLINE,		offline)	\
	x(DURATION_LIMIT,	duration_limit)	\
	x(INVAL,		inval)		\
	x(REMOVED,		removed)

enum bch_sb_error_id bch2_blk_sts_sb_err(int);

/*
 * Decompression failures we count separately: an extent that won't
 * decompress is a different animal from one that won't read or won't
 * checksum - as far as the checksum can tell the data came off the device
 * intact, and then didn't decode. Counting per compression type answers
 * the first question such a report raises, which is whether one algorithm
 * is at fault or all of them are.
 *
 * The zstd codes separate that further: corruption_detected and
 * checksum_wrong say the compressed data is bad, dst_size_too_small and
 * memory_allocation say we drove zstd wrong or couldn't allocate.
 *
 * Curated by hand for the same reason as the block statuses above - sb
 * error ids are permanent on-disk numbers. zstd defines 27 error codes,
 * most of which zstd_decompress_dctx() can't return for the single raw
 * frame we hand it; those count as data_decompress_err_zstd_unknown, and
 * anything not a decompress errcode at all as data_decompress_err_unknown.
 * To promote one, add it here and add a numbered entry in
 * errors_format.h - the build breaks if you forget.
 */
#define BCH_DECOMPRESS_SB_ERRS()						\
	x(decompress_exceeded_max_encoded_extent, exceeded_max_encoded_extent)	\
	x(decompress_lz4_old,			lz4_old)			\
	x(decompress_lz4,			lz4)				\
	x(decompress_gzip,			gzip)				\
	x(decompress_gzip_size_mismatch,	gzip_size_mismatch)		\
	x(decompress_zstd_src_len_bad,		zstd_src_len_bad)		\
	x(decompress_zstd_size_mismatch,	zstd_size_mismatch)		\
	x(ZSTD_error_corruption_detected,	zstd_corruption_detected)	\
	x(ZSTD_error_checksum_wrong,		zstd_checksum_wrong)		\
	x(ZSTD_error_prefix_unknown,		zstd_prefix_unknown)		\
	x(ZSTD_error_srcSize_wrong,		zstd_src_size_wrong)		\
	x(ZSTD_error_dstSize_tooSmall,		zstd_dst_size_too_small)	\
	x(ZSTD_error_memory_allocation,		zstd_memory_allocation)

enum bch_sb_error_id bch2_decompress_sb_err(int);

void bch2_sb_errors_from_cpu(struct bch_fs *);
int bch2_sb_errors_to_cpu(struct bch_fs *);

#endif /* _BCACHEFS_SB_ERRORS_H */
