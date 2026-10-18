/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_SYSFS_H_
#define _BCACHEFS_SYSFS_H_

#include <linux/sysfs.h>

struct attribute;
struct sysfs_ops;

#include "debug/sysfs_gen.h"

extern struct attribute *bch2_dev_files[];

extern const struct sysfs_ops bch2_dev_sysfs_ops;

#endif  /* _BCACHEFS_SYSFS_H_ */
