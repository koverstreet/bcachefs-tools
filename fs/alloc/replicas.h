/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_REPLICAS_H
#define _BCACHEFS_REPLICAS_H

#include "btree/bkey.h"
#include "alloc/replicas_gen.h"
#include "data_structures/eytzinger.h"


static inline bool bch2_replicas_entry_eq(struct bch_replicas_entry_v1 *l,
					  struct bch_replicas_entry_v1 *r)
{
	return l->nr_devs == r->nr_devs && !memcmp(l, r, replicas_entry_bytes(l));
}

#endif /* _BCACHEFS_REPLICAS_H */
