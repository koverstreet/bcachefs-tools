/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_VFS_FDM_TYPES_H
#define _BCACHEFS_VFS_FDM_TYPES_H

#define FDM_NR_HASH		3

#define FDM_HASH_BITS		9

#define FDM_HASH_SIZE		(1 << FDM_HASH_BITS)

struct fdm_slot {
	struct task_struct	*task;
	unsigned long		mapping;	/* address_space * | dropped_locks bit */
};

struct fdm_hash {
	u64			hash_seeds[FDM_NR_HASH];
	struct closure_waitlist	wait;
	struct fdm_slot		slots[FDM_HASH_SIZE];
};

#endif /* _BCACHEFS_VFS_FDM_TYPES_H */
