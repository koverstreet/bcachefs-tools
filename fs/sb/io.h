/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_SB_IO_H
#define _BCACHEFS_SB_IO_H

#include "data/extents.h"
#include "init/dev_gen.h"
#include "sb/members.h"
#include "data_structures/eytzinger.h"

#include <asm/byteorder.h>

static inline __le64 bch2_sb_magic(struct bch_fs *c)
{
	__le64 ret;

	memcpy(&ret, &c->sb.uuid, sizeof(ret));
	return ret;
}

static inline __u64 jset_magic(struct bch_fs *c)
{
	return __le64_to_cpu(bch2_sb_magic(c) ^ JSET_MAGIC);
}

static inline __u64 bset_magic(struct bch_fs *c)
{
	return __le64_to_cpu(bch2_sb_magic(c) ^ BSET_MAGIC);
}

#endif /* _BCACHEFS_SB_IO_H */
