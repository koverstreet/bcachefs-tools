/* SPDX-License-Identifier: GPL-2.0 */

#ifndef _BCACHEFS_DATA_UPDATE_H
#define _BCACHEFS_DATA_UPDATE_H

#include "btree/bkey_buf.h"
#include "btree/update.h"
#include "data/read.h"
#include "data/write_gen.h"

#include "data/update_gen.h"
#include "data/update_feasible_gen.h"
#include "data/update_index_gen.h"

/* Both scrub types read to check, and must never rewrite what they're checking */
static inline bool data_update_is_scrub(enum bch_data_update_types type)
{
	return type == BCH_DATA_UPDATE_scrub ||
	       type == BCH_DATA_UPDATE_scrub_no_repair;
}

#endif /* _BCACHEFS_DATA_UPDATE_H */
