/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_STR_HASH_H
#define _BCACHEFS_STR_HASH_H

#include "btree/iter.h"
#include "btree/update.h"

#include "data/checksum.h"

#include "fs/inode.h"

#include "init/error.h"

#include "snapshots/subvolume.h"

#include "util/siphash.h"

#include <crypto/sha2.h>

static inline enum bch_str_hash_type
bch2_str_hash_opt_to_type(struct bch_fs *c, enum bch_str_hash_opts opt)
{
	switch (opt) {
	case BCH_STR_HASH_OPT_crc32c:
		return BCH_STR_HASH_crc32c;
	case BCH_STR_HASH_OPT_crc64:
		return BCH_STR_HASH_crc64;
	case BCH_STR_HASH_OPT_siphash:
		return c->sb.features & (1ULL << BCH_FEATURE_new_siphash)
			? BCH_STR_HASH_siphash
			: BCH_STR_HASH_siphash_old;
	default:
	     BUG();
	}
}

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

struct bch_hash_info __bch2_hash_info_init(struct bch_fs *, const struct bch_inode_unpacked *);
int bch2_hash_info_init(struct bch_fs *, const struct bch_inode_unpacked *, struct bch_hash_info *);

#endif /* _BCACHEFS_STR_HASH_H */
