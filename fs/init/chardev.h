/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_CHARDEV_H
#define _BCACHEFS_CHARDEV_H

#ifndef NO_BCACHEFS_FS

#include "init/chardev_gen.h"

#else

static inline long bch2_fs_ioctl(struct bch_fs *c,
				unsigned cmd, void __user * arg)
{
	return -ENOTTY;
}


static inline void bch2_chardev_exit(void) {}
static inline int __init bch2_chardev_init(void) { return 0; }

#endif /* NO_BCACHEFS_FS */

#endif /* _BCACHEFS_CHARDEV_H */
