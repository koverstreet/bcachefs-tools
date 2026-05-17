/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_BITFLIP_REPAIR_H
#define _BCACHEFS_BITFLIP_REPAIR_H

#include "bcachefs.h"

struct bio;
struct bch_extent_crc_unpacked;

int bch2_try_bitflip_repair_bio(struct bch_fs *, struct bio *,
				struct bch_extent_crc_unpacked *,
				struct bch_csum expected);

#endif /* _BCACHEFS_BITFLIP_REPAIR_H */
