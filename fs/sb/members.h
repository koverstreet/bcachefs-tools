/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_SB_MEMBERS_H
#define _BCACHEFS_SB_MEMBERS_H

#include "btree/bkey_gen.h"
#include "sb/members_gen.h"
#include "util/enumerated_ref.h"
#include "data_structures/darray.h"

/* Superblock member access: */

static inline struct bch_member *
__bch2_members_v2_get_mut(struct bch_sb_field_members_v2 *mi, unsigned i)
{
	return (void *) mi->_members + (i * le16_to_cpu(mi->member_bytes));
}

/* Device lookup: */

static inline bool bch2_dev_exists(const struct bch_fs *c, unsigned dev)
{
	return dev < c->sb.nr_devices && c->devs[dev];
}

static inline bool bucket_valid(const struct bch_dev *ca, u64 b)
{
	return b - ca->mi.first_bucket < ca->mi.nbuckets_minus_first;
}

static inline struct bch_dev *bch2_dev_have_ref(const struct bch_fs *c, unsigned dev)
{
	if (WARN(!bch2_dev_exists(c, dev), "nonexistent dev %u (nr_devices %u)", dev, c->sb.nr_devices))
		return NULL;

	return rcu_dereference_check(c->devs[dev], 1);
}

static inline struct bch_dev *bch2_dev_rcu_noerror(const struct bch_fs *c, unsigned dev)
{
	return c && dev < c->sb.nr_devices
		? rcu_dereference(c->devs[dev])
		: NULL;
}

static inline struct bch_dev *bch2_dev_get_ioref(struct bch_fs *c, unsigned dev,
						 int rw, unsigned ref_idx)
{
	might_sleep();

	guard(rcu)();
	struct bch_dev *ca = bch2_dev_rcu_noerror(c, dev);
	if (!ca || !enumerated_ref_tryget(&ca->io_ref[rw], ref_idx))
		return NULL;

	if (ca->mi.state == BCH_MEMBER_STATE_rw || rw == READ)
		return ca;

	enumerated_ref_put(&ca->io_ref[rw], ref_idx);
	return NULL;
}

#endif /* _BCACHEFS_SB_MEMBERS_H */
