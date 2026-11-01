/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_SUPER_H
#define _BCACHEFS_SUPER_H

#include "data/extents.h"
#include "init/dev_gen.h"
#include "util/darray.h"

#include "bcachefs_ioctl.h"

#include <linux/math64.h>
#include "init/fs_gen.h"
#include "init/rw_gen.h"

#define KTYPE(type)							\
static const struct attribute_group type ## _group = {			\
	.attrs = type ## _files						\
};									\
									\
static const struct attribute_group *type ## _groups[] = {		\
	&type ## _group,						\
	NULL								\
};									\
									\
const struct kobj_type type ## _ktype = {				\
	.release	= type ## _release,				\
	.sysfs_ops	= &type ## _sysfs_ops,				\
	.default_groups = type ## _groups				\
}

extern const char * const bch2_fs_flag_strs[];
extern const char * const bch2_write_refs[];
extern const char * const bch2_dev_read_refs[];
extern const char * const bch2_dev_write_refs[];

int bch2_fs_resize_on_mount(struct bch_fs *);

#endif /* _BCACHEFS_SUPER_H */
