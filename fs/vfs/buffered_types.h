/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_VFS_BUFFERED_TYPES_H
#define _BCACHEFS_VFS_BUFFERED_TYPES_H

#ifndef NO_BCACHEFS_FS

#include <linux/version.h>

#include "data/write_types.h"

struct bch_writepage_io {
	struct bch_inode_info		*inode;

	/* must be last: */
	struct bch_write_op		op;
};

#endif

#endif /* _BCACHEFS_VFS_BUFFERED_TYPES_H */
