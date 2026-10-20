// SPDX-License-Identifier: GPL-2.0

#include "bcachefs.h"

#include "sb/errors.h"
#include "sb/io.h"

#include "util/darray.h"

/* @err is a BCH_ERR_BLK_STS_* errcode, as thrown by blk_status_to_bch_err() */
enum bch_sb_error_id bch2_blk_sts_sb_err(int err)
{
	switch (abs(err)) {
#define x(_blk_sts, _name)						\
	case BCH_ERR_BLK_STS_##_blk_sts:				\
		return BCH_FSCK_ERR_blk_sts_##_name;
	BCH_BLK_STS_SB_ERRS()
#undef x
	default:
		return BCH_FSCK_ERR_blk_sts_unknown;
	}
}

/* @err is a BCH_ERR_decompress errcode, as thrown by buf_uncompress() */
enum bch_sb_error_id bch2_decompress_sb_err(int err)
{
	switch (abs(err)) {
#define x(_errcode, _name)						\
	case BCH_ERR_##_errcode:					\
		return BCH_FSCK_ERR_data_decompress_err_##_name;
	BCH_DECOMPRESS_SB_ERRS()
#undef x
	default:
		return bch2_err_matches(err, BCH_ERR_zstd_error)
			? BCH_FSCK_ERR_data_decompress_err_zstd_unknown
			: BCH_FSCK_ERR_data_decompress_err_unknown;
	}
}

/*
 * Same errcode, same table, same names as bch2_decompress_sb_err() above - the
 * reason we stamp into a KEY_TYPE_error key when this is what we finally gave
 * up on. The sb counter says how often decompression failed this way; the key
 * says which extent it cost us.
 */
enum bch_key_type_errors bch2_decompress_key_type_error(int err)
{
	switch (abs(err)) {
#define x(_errcode, _name)						\
	case BCH_ERR_##_errcode:					\
		return KEY_TYPE_ERROR_decompress_##_name;
	BCH_DECOMPRESS_SB_ERRS()
#undef x
	default:
		return bch2_err_matches(err, BCH_ERR_zstd_error)
			? KEY_TYPE_ERROR_decompress_zstd_unknown
			: KEY_TYPE_ERROR_decompress_unknown;
	}
}
