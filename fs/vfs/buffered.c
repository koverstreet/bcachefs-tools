// SPDX-License-Identifier: GPL-2.0
#ifndef NO_BCACHEFS_FS

#include "bcachefs.h"

#include "btree/bkey_buf.h"

#include "data/read.h"

#include "snapshots/snapshot.h"

#include "vfs/io.h"
#include "vfs/buffered.h"
#include "vfs/direct.h"
#include "vfs/pagecache.h"

#include <linux/backing-dev.h>
#include <linux/pagemap.h>
#include <linux/writeback.h>
#include <linux/version.h>

int bch2_read_folio(struct file *file, struct folio *folio)
{
	int ret;

	ret = bch2_read_single_folio(folio, folio->mapping, false);
	folio_unlock(folio);
	return bch2_err_class(ret);
}

#endif /* NO_BCACHEFS_FS */
