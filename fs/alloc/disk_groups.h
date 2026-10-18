/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_DISK_GROUPS_H
#define _BCACHEFS_DISK_GROUPS_H

#include "init/dev_gen.h"
#include "alloc/disk_groups_gen.h"
#include "alloc/disk_groups_defs_gen.h"

static inline u16 dev_to_target(unsigned dev)
{
	return TARGET_DEV_START + dev;
}

static inline u16 group_to_target(unsigned group)
{
	return TARGET_GROUP_START + group;
}

static inline struct target target_decode(unsigned target)
{
	if (target >= TARGET_GROUP_START)
		return (struct target) {
			.type	= TARGET_GROUP,
			.group	= target - TARGET_GROUP_START
		};

	if (target >= TARGET_DEV_START)
		return (struct target) {
			.type	= TARGET_DEV,
			.group	= target - TARGET_DEV_START
		};

	return (struct target) { .type = TARGET_NULL };
}

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

static inline bool bch2_target_accepts_data(struct bch_fs *c,
					    enum bch_data_type data_type,
					    u16 target)
{
	struct bch_devs_mask rw_devs = target_rw_devs(c, data_type, target);
	return !bitmap_empty(rw_devs.d, BCH_SB_MEMBERS_MAX);
}

static inline bool bch2_dev_in_target(struct bch_fs *c, unsigned dev, unsigned target)
{
	guard(rcu)();
	return bch2_dev_in_target_rcu(c, dev, target);
}

#define bch2_opt_target (struct bch_opt_fn) {		\
	.parse		= bch2_opt_target_parse,	\
	.to_text	= bch2_opt_target_to_text,	\
}

#define bch2_opt_disk_label (struct bch_opt_fn) {	\
	.parse		= bch2_opt_disk_label_parse,	\
	.to_text	= bch2_opt_disk_label_to_text,	\
}

#endif /* _BCACHEFS_DISK_GROUPS_H */
