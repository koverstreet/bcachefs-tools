/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_ACL_H
#define _BCACHEFS_ACL_H

#include "fs/acl_gen.h"

#define BCH_ACL_VERSION	0x0001

#ifndef NO_BCACHEFS_FS

struct posix_acl *bch2_get_acl(struct inode *, int, bool);

int bch2_set_acl(struct mnt_idmap *, struct dentry *, struct posix_acl *, int);
int bch2_acl_chmod(struct btree_trans *, subvol_inum,
		   struct bch_inode_unpacked *,
		   umode_t, struct posix_acl **);

#else

static inline int bch2_acl_chmod(struct btree_trans *trans, subvol_inum inum,
				 struct bch_inode_unpacked *inode,
				 umode_t mode,
				 struct posix_acl **new_acl)
{
	return 0;
}

#endif /* NO_BCACHEFS_FS */

#endif /* _BCACHEFS_ACL_H */
