/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_ERRCODE_H
#define _BCACHEFS_ERRCODE_H

/* we're getting away from reusing bi_status, this should go away */
#define BLK_STS_REMOVED		((__force blk_status_t)128)

#include "errcode_types.h"

__attribute__((const)) const char *bch2_err_str(int);

__attribute__((const)) bool __bch2_err_matches(int, int);

__attribute__((const))
static inline bool _bch2_err_matches(int err, int class)
{
	return err < 0 && __bch2_err_matches(err, class);
}

#define bch2_err_matches(_err, _class)			\
({							\
	BUILD_BUG_ON(!__builtin_constant_p(_class));	\
	unlikely(_bch2_err_matches(_err, _class));	\
})

int __bch2_err_class(int);

static inline s64 bch2_err_class(s64 err)
{
	return err < 0 ? __bch2_err_class(err) : err;
}

#include <linux/blk_types.h>
const char *bch2_blk_status_to_str(blk_status_t);
enum bch_errcode blk_status_to_bch_err(blk_status_t);

#include <linux/zstd_errors.h>

enum bch_errcode zstd_err_to_bch_err(ZSTD_ErrorCode);

#endif /* _BCACHFES_ERRCODE_H */
