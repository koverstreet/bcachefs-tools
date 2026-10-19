/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_REPLICAS_H
#define _BCACHEFS_REPLICAS_H

#include "btree/bkey.h"
#include "alloc/replicas_gen.h"
#include "util/eytzinger.h"

static inline void bch2_replicas_entry_cached(struct bch_replicas_entry_v1 *e,
					      unsigned dev)
{
	e->data_type	= BCH_DATA_cached;
	e->nr_devs	= 1;
	e->nr_required	= 1;
	e->devs[0]	= dev;
}

#include "alloc/replicas_defs_gen.h"

static inline void bch2_replicas_entry_put(struct bch_fs *c, struct bch_replicas_entry_v1 *r)
{
	bch2_replicas_entry_put_many(c, r, 1);
}

static inline bool bch2_replicas_entry_has_dev(struct bch_replicas_entry_v1 *r, unsigned dev)
{
	for (unsigned i = 0; i < r->nr_devs; i++)
		if (r->devs[i] == dev)
			return true;
	return false;
}

static inline bool bch2_replicas_entry_eq(struct bch_replicas_entry_v1 *l,
					  struct bch_replicas_entry_v1 *r)
{
	return l->nr_devs == r->nr_devs && !memcmp(l, r, replicas_entry_bytes(l));
}

#endif /* _BCACHEFS_REPLICAS_H */
