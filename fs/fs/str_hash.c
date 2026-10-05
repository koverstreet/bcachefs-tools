// SPDX-License-Identifier: GPL-2.0

#include "bcachefs.h"

#include "fs/str_hash.h"

struct bch_hash_info
__bch2_hash_info_init(struct bch_fs *c, const struct bch_inode_unpacked *bi)
{
	struct bch_hash_info info = {
		.inum_snapshot	= bi->bi_snapshot,
		.type		= INODE_STR_HASH(bi),
		.is_31bit	= bi->bi_flags & BCH_INODE_31bit_dirent_offset,
		.cf_encoding	= bch2_inode_casefold(c, bi) ? c->cf_encoding : NULL,
		.siphash_key	= { .k0 = bi->bi_hash_seed }
	};

	if (unlikely(info.type == BCH_STR_HASH_siphash_old)) {
		u8 digest[SHA256_DIGEST_SIZE];

		sha256((const u8 *)&bi->bi_hash_seed,
		       sizeof(bi->bi_hash_seed), digest);
		memcpy(&info.siphash_key, digest, sizeof(info.siphash_key));
	}

	return info;
}

int bch2_hash_info_init(struct bch_fs *c, const struct bch_inode_unpacked *bi,
			struct bch_hash_info *ret)
{
	if (bch2_inode_casefold(c, bi) && !c->cf_encoding)
		return bch_err_throw(c, casefold_dir_but_disabled);

	*ret = __bch2_hash_info_init(c, bi);
	return 0;
}
