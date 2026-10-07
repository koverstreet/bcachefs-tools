/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_VFS_IO_TYPES_H
#define _BCACHEFS_VFS_IO_TYPES_H

#ifndef NO_BCACHEFS_FS

#include "alloc/buckets.h"

#include "data/write_types.h"

#include "fs/quota.h"

#include "vfs/fdm.h"

#include "vfs/fs.h"

#include <linux/uio.h>

struct nocow_flush {
	struct closure	*cl;
	struct bch_dev	*ca;
	struct bio	bio;
};

struct folio_vec {
	struct folio	*fv_folio;
	size_t		fv_offset;
	size_t		fv_len;
};

struct quota_res {
	u64				sectors;
};

#endif

#endif /* _BCACHEFS_VFS_IO_TYPES_H */
