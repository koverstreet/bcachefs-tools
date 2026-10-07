/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_FS_STR_HASH_TYPES_H
#define _BCACHEFS_FS_STR_HASH_TYPES_H

struct bch_hash_info {
	u32			inum_snapshot;
	u8			type;
	bool			is_31bit;
	struct unicode_map	*cf_encoding;
	/*
	 * For crc32 or crc64 string hashes the first key value of
	 * the siphash_key (k0) is used as the key.
	 */
	SIPHASH_KEY	siphash_key;
};

#endif /* _BCACHEFS_FS_STR_HASH_TYPES_H */
