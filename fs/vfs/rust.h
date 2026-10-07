/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_VFS_RUST_H
#define _BCACHEFS_VFS_RUST_H

/*
 * The VFS, for Rust - out of line, as util/locking.h's shims are.
 *
 * struct bch_inode_info is kernel types all the way down - struct inode,
 * locks, the pagecache - so Rust doesn't bind its layout: it's opaque there,
 * and reached through these. Nor does Rust call kernel functions that aren't
 * in the kernel crate's bindings, posix_acl's among them: those are here too.
 */

#include "snapshots/types_gen.h"

#ifndef NO_BCACHEFS_FS

#include "vfs/rust_gen.h"

struct bch_fs;
struct bch_inode_info;
struct bch_inode_unpacked;
struct btree_trans;
struct dentry;
struct inode;
struct mnt_idmap;
struct posix_acl;
struct xattr_handler;

/* bch_inode_info: */
struct bch_inode_info *rust_to_bch_ei(struct inode *);
struct inode *rust_ei_vinode(struct bch_inode_info *);
struct bch_fs *rust_ei_fs(struct bch_inode_info *);
struct bch_inode_unpacked *rust_ei_inode(struct bch_inode_info *);
subvol_inum rust_ei_inum(struct bch_inode_info *);
void rust_ei_update_lock(struct bch_inode_info *);
void rust_ei_update_unlock(struct bch_inode_info *);
int rust_ei_set_projid(struct bch_fs *, struct bch_inode_info *, u32);

/* dentries: */
struct bch_inode_info *rust_dentry_ei(struct dentry *);
bool rust_dentry_parent_inode_opt(struct dentry *, unsigned, u64 *);
void rust_dir_casefold_changed(struct dentry *);

/* vfs/fs.c, as vfs/fs.h declares them: */
typedef int (*inode_set_fn)(struct btree_trans *,
			    struct bch_inode_info *,
			    struct bch_inode_unpacked *, void *);

void bch2_inode_update_after_write(struct btree_trans *,
				   struct bch_inode_info *,
				   struct bch_inode_unpacked *,
				   unsigned);
int __must_check bch2_write_inode(struct bch_fs *, struct bch_inode_info *,
				  inode_set_fn, void *, unsigned);
int bch2_inode_or_descendents_is_open(struct btree_trans *, struct bpos);

/* xattr handlers: */
int rust_xattr_handler_flags(const struct xattr_handler *);

/* posix ACLs: */
struct posix_acl *rust_posix_acl_alloc(struct btree_trans *, unsigned);
unsigned rust_posix_acl_count(const struct posix_acl *);
void rust_posix_acl_entry(const struct posix_acl *, unsigned, u16 *, u16 *, u32 *);
void rust_posix_acl_set_entry(struct posix_acl *, unsigned, u16, u16, u32);
void rust_posix_acl_release(struct posix_acl *);
void rust_set_cached_acl(struct inode *, int, struct posix_acl *);
int rust_posix_acl_update_mode(struct mnt_idmap *, struct inode *, umode_t *,
			       struct posix_acl **);
int rust_posix_acl_chmod(struct btree_trans *, struct posix_acl **, umode_t);

#endif /* NO_BCACHEFS_FS */

#endif /* _BCACHEFS_VFS_RUST_H */
