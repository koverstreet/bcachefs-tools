/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_DISK_GROUPS_H
#define _BCACHEFS_DISK_GROUPS_H

#include "init/dev_gen.h"
#include "alloc/disk_groups_gen.h"

static inline struct bch_devs_mask target_rw_devs(struct bch_fs *c,
						  enum bch_data_type data_type,
						  u16 target)
{
	struct bch_devs_mask devs = c->allocator.rw_devs[data_type];

	guard(rcu)();
	const struct bch_devs_mask *t = bch2_target_to_mask(c, target);

	if (t)
		bitmap_and(devs.d, devs.d, t->d, BCH_SB_MEMBERS_MAX);
	return devs;
}

#endif /* _BCACHEFS_DISK_GROUPS_H */
