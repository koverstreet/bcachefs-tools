// SPDX-License-Identifier: GPL-2.0
#ifndef NO_BCACHEFS_FS

#include "bcachefs.h"
#include "vfs/buffered.h"
#include "vfs/direct.h"
#include "vfs/fs.h"
#include "vfs/swap.h"

#include <linux/swap.h>

int bch2_swap_activate(struct swap_info_struct *sis,
		       struct file *file, sector_t *span)
{
	struct bch_inode_info *inode = file_bch_inode(file);

	if (!S_ISREG(inode->v.i_mode))
		return -EINVAL;

	sis->flags |= SWP_FS_OPS;
	*span = sis->pages;

	return add_swap_extent(sis, 0, sis->max, 0);
}

void bch2_swap_deactivate(struct file *file)
{
}

int bch2_swap_rw(struct kiocb *iocb, struct iov_iter *iter)
{
	iocb->ki_flags |= IOCB_DIRECT;

	return iov_iter_rw(iter) == READ
		? bch2_read_iter(iocb, iter)
		: bch2_write_iter(iocb, iter);
}

#endif /* NO_BCACHEFS_FS */
