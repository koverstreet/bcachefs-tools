/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_ACL_H
#define _BCACHEFS_ACL_H

#include "fs/acl_gen.h"

struct bch_inode_unpacked;
struct posix_acl;

#ifdef NO_BCACHEFS_FS

static inline int bch2_acl_chmod(struct btree_trans *trans, subvol_inum inum,
				 const struct bch_inode_unpacked *inode,
				 umode_t mode,
				 struct posix_acl **new_acl)
{
	return 0;
}

#endif /* NO_BCACHEFS_FS */

#endif /* _BCACHEFS_ACL_H */
