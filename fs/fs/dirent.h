/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_DIRENT_H
#define _BCACHEFS_DIRENT_H

#include "str_hash.h"

int bch2_dirent_validate(struct bch_fs *, struct bkey_s_c,
			 const struct bkey_validate_context *);
void bch2_dirent_to_text(struct printbuf *, struct bch_fs *, struct bkey_s_c);

#define bch2_bkey_ops_dirent ((struct bkey_ops) {	\
	.key_validate	= bch2_dirent_validate,		\
	.val_to_text	= bch2_dirent_to_text,		\
	.min_val_size	= 16,				\
})

#include "fs/dirent_types.h"

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

struct qstr bch2_dirent_get_name(struct bkey_s_c_dirent);

struct bkey_s_c bch2_dirent_lookup_key(struct btree_trans *, struct btree_iter *,
				       subvol_inum, const struct bch_hash_info *,
				       const struct qstr *);

int bch2_dirent_read_target(struct btree_trans *, subvol_inum,
			    struct bkey_s_c_dirent, subvol_inum *);

static inline unsigned vfs_d_type(unsigned type)
{
	return type == DT_SUBVOL ? DT_DIR : type;
}

int bch2_dirent_lookup(struct bch_fs *, subvol_inum,
		       const struct bch_hash_info *,
		       const struct qstr *, subvol_inum *);

int bch2_readdir(struct bch_fs *, subvol_inum, struct bch_hash_info *, struct dir_context *);

/* readdir's VFS end, in C - fs/dirent.c: */
void bch2_readdir_fault_in(struct dir_context *);
int bch2_dir_emit(struct btree_trans *, struct dir_context *,
		  struct bkey_s_c_dirent, subvol_inum);

void bch2_dirent_init(void);
void bch2_filldir64_specialization_to_text(struct printbuf *);

#endif /* _BCACHEFS_DIRENT_H */
