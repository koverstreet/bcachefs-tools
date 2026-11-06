/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_DIRENT_H
#define _BCACHEFS_DIRENT_H

#include "str_hash.h"

#include "fs/dirent_gen.h"

#if IS_ENABLED(CONFIG_UNICODE)
/*
 * utf8_casefold() under @info's encoding, for Rust: kernel headers aren't
 * bound in a kernel build, so it can't call utf8_casefold() itself.
 */
static inline int bch2_utf8_casefold(const struct bch_hash_info *info,
				     const struct qstr *str,
				     unsigned char *dest, size_t dlen)
{
	return utf8_casefold(info->cf_encoding, str, dest, dlen);
}
#endif

static inline unsigned vfs_d_type(unsigned type)
{
	return type == DT_SUBVOL ? DT_DIR : type;
}

void bch2_dirent_init(void);
void bch2_filldir64_specialization_to_text(struct printbuf *);

#endif /* _BCACHEFS_DIRENT_H */
