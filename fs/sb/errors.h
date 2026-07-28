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

void bch2_sb_errors_from_cpu(struct bch_fs *);
int bch2_sb_errors_to_cpu(struct bch_fs *);

#endif /* _BCACHEFS_SB_ERRORS_H */
