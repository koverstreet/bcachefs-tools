/*
 * Block data types and constants.  Directly include this file only to
 * break include dependency loop.
 */
#ifndef __LINUX_BLK_TYPES_H
#define __LINUX_BLK_TYPES_H

#include <linux/atomic.h>
#include <linux/backing-dev.h>
#include <linux/types.h>
#include <linux/bvec.h>
#include <linux/mutex.h>

struct inode {
	unsigned long		i_ino;
	loff_t			i_size;
	struct super_block	*i_sb;
	unsigned int		mode;
	void			*i_private;
	unsigned		i_state;
};

/* os/linux_user/blkdev.rs's BlockDevice; struct bio is os/linux_user/c.rs's */
struct block_device;

#endif /* __LINUX_BLK_TYPES_H */
