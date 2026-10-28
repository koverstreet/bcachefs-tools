// SPDX-License-Identifier: GPL-2.0
/*
 * buckets.c's last: the allocator's check for superblock and journal buckets,
 * which reads the device's superblock layout without sb_lock.
 *
 * Copyright 2014 Datera, Inc.
 */

#include "bcachefs.h"

#include "alloc/buckets.h"

bool bch2_is_superblock_bucket(struct bch_dev *ca, u64 b)
{
	struct bch_sb_layout *layout = &ca->disk_sb.sb->layout;
	u64 b_offset	= bucket_to_sector(ca, b);
	u64 b_end	= bucket_to_sector(ca, b + 1);
	unsigned i;

	if (!b)
		return true;

	for (i = 0; i < layout->nr_superblocks; i++) {
		u64 offset = le64_to_cpu(layout->sb_offset[i]);
		u64 end = offset + (1 << layout->sb_max_size_bits);

		if (!(offset >= b_end || end <= b_offset))
			return true;
	}

	for (i = 0; i < ca->journal.nr; i++)
		if (b == ca->journal.buckets[i])
			return true;

	return false;
}
