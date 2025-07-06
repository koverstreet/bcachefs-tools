/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _POSIX_TO_BCACHEFS_H
#define _POSIX_TO_BCACHEFS_H

/*
 * This header exports the functionality needed for copying data from existing
 * posix compliant filesystems to bcachefs. There are two use cases:
 * 1. Creating a new bcachefs filesystem using `bcachefs format`, we can
 *    specify a source directory tree which will be copied over the new
 *    bcachefs filesytem.
 * 2. Migrating an existing filesystem in place, with `bcachefs migrate`.
 *    This will allocate space for the bcachefs metadata, but the actual data
 *    represented by the extents will not be duplicated. The bcachefs metadata
 *    will simply point to the existing extents.
 *
 * To avoid code duplication, `copy_fs` deals with both cases. See the function
 * documentation for more details.
 */

#include "libbcachefs.h"

enum bch_migrate_type {
	BCH_MIGRATE_copy,
	BCH_MIGRATE_migrate
};

/*
 * The migrate action uses all the fields in this struct.
 * The copy action only uses the `hardlinks` field. Since `hardlinks` is
 * initialized with zeroes, an empty `copy_fs_state` struct can be passed.
 */
struct copy_fs_state {
	u64			bcachefs_inum;
	dev_t			dev;

	GENRADIX(u64)		hardlinks;
	ranges			extents;
	enum bch_migrate_type	type;

	u64			reserve_start;

	u64			total_input;
	u64			total_wrote;
	u64			total_linked;
};

/*
 * The `copy_fs` function is used for both copying a directory tree to a new
 * bcachefs filesystem and migrating an existing one, depending on the value
 * from the `type` field in `copy_fs_state` struct.
 *
 * In case of copy, an empty `copy_fs_state` structure is passed to `copy_fs`
 * (only the `hardlinks` field is used, and that is initialized with zeroes).
 *
 * In the migrate case, all the fields from `copy_fs_state` need to be
 * initialized (`hardlinks` is initialized with zeroes).
 */
int copy_fs(struct bch_fs *, struct copy_fs_state *, int, const char *);

#endif /* _LIBBCACHE_H */
