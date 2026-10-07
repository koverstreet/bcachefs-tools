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

struct qstr;
struct file;
struct dir_context;
struct bch_fs;
struct bch_hash_info;
struct bch_inode_info;

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
int bch2_dirent_delete_at(struct btree_trans *, const struct bch_hash_info *,
			  struct btree_iter *, enum btree_iter_update_trigger_flags);

static inline struct bkey_s_c_dirent dirent_get_by_pos(struct btree_trans *trans,
						struct btree_iter *iter,
						struct bpos pos)
{
	bch2_trans_iter_init(trans, iter, BTREE_ID_dirents, pos, 0);
	return bch2_bkey_get_typed(iter, dirent);
}

int bch2_dirent_read_target(struct btree_trans *, subvol_inum,
			    struct bkey_s_c_dirent, subvol_inum *);

static inline void dirent_copy_target(struct bkey_i_dirent *dst,
				      struct bkey_s_c_dirent src)
{
	dst->v.d_inum = src.v->d_inum;
	dst->v.d_type = src.v->d_type;
}


int bch2_dirent_create_snapshot(struct btree_trans *, u32, u32,
				struct bch_inode_unpacked *dir_u,
				u8, const struct qstr *, u64, u64 *,
				enum btree_iter_update_trigger_flags);
int bch2_dirent_create(struct btree_trans *, subvol_inum,
		       struct bch_inode_unpacked *dir_u,
		       u8, const struct qstr *, u64, u64 *,
		       enum btree_iter_update_trigger_flags);

static inline unsigned vfs_d_type(unsigned type)
{
	return type == DT_SUBVOL ? DT_DIR : type;
}

enum bch_rename_mode {
	BCH_RENAME,
	BCH_RENAME_OVERWRITE,
	BCH_RENAME_EXCHANGE,
};

int bch2_dirent_rename(struct btree_trans *,
		       subvol_inum, struct bch_hash_info *,
		       subvol_inum, struct bch_hash_info *,
		       const struct qstr *, subvol_inum *, u64 *,
		       const struct qstr *, subvol_inum *, u64 *,
		       enum bch_rename_mode);

int bch2_dirent_lookup_snapshot(struct btree_trans *,
				struct btree_iter *,
				subvol_inum, u32,
				const struct bch_hash_info *,
				const struct qstr *, subvol_inum *,
				unsigned);

int bch2_dirent_lookup(struct bch_fs *, subvol_inum,
		       const struct bch_hash_info *,
		       const struct qstr *, subvol_inum *);

int bch2_empty_dir_snapshot(struct btree_trans *, u64, u32, u32);
int bch2_empty_dir_trans(struct btree_trans *, subvol_inum);
int bch2_readdir(struct bch_fs *, subvol_inum, struct bch_hash_info *, struct dir_context *);

/* readdir's VFS end, in C - fs/dirent.c: */
void bch2_readdir_fault_in(struct dir_context *);
int bch2_dir_emit(struct btree_trans *, struct dir_context *,
		  struct bkey_s_c_dirent, subvol_inum);

void bch2_dirent_init(void);
void bch2_filldir64_specialization_to_text(struct printbuf *);

int bch2_fsck_remove_dirent(struct btree_trans *, struct bpos);

#endif /* _BCACHEFS_DIRENT_H */
