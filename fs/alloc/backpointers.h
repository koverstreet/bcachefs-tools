/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_BACKPOINTERS_H
#define _BCACHEFS_BACKPOINTERS_H

#include "alloc/buckets.h"

#include "btree/cache.h"
#include "btree/iter.h"
#include "btree/update.h"

#include "data/reconcile/trigger.h"

#include "init/error.h"

static inline u64 swab40(u64 x)
{
	return (((x & 0x00000000ffULL) << 32)|
		((x & 0x000000ff00ULL) << 16)|
		((x & 0x0000ff0000ULL) >>  0)|
		((x & 0x00ff000000ULL) >> 16)|
		((x & 0xff00000000ULL) >> 32));
}

#define bch2_bkey_ops_backpointer ((struct bkey_ops) {	\
	.key_validate	= bch2_backpointer_validate,	\
	.val_to_text	= bch2_backpointer_to_text,	\
	.swab		= bch2_backpointer_swab,	\
})

/*
 * Convert from pos in backpointer btree to pos of corresponding bucket in alloc
 * btree:
 */
static inline struct bpos bp_pos_to_bucket(const struct bch_dev *ca, struct bpos bp_pos)
{
	u64 bucket_sector = bp_pos.offset >> ca->fs->sb.extent_bp_shift;

	return POS(bp_pos.inode, sector_to_bucket(ca, bucket_sector));
}

static inline struct bpos bucket_pos_to_bp_noerror(const struct bch_dev *ca,
						   struct bpos bucket,
						   u64 bucket_offset)
{
	return POS(bucket.inode,
		   (bucket_to_sector(ca, bucket.offset) <<
		    ca->fs->sb.extent_bp_shift) + bucket_offset);
}

/*
 * Convert from pos in alloc btree + bucket offset to pos in backpointer btree:
 */
static inline struct bpos bucket_pos_to_bp(const struct bch_dev *ca,
					   struct bpos bucket,
					   u64 bucket_offset)
{
	struct bpos ret = bucket_pos_to_bp_noerror(ca, bucket, bucket_offset);
	EBUG_ON(!bkey_eq(bucket, bp_pos_to_bucket(ca, ret)));
	return ret;
}

static inline struct bpos bucket_pos_to_bp_start(const struct bch_dev *ca, struct bpos bucket)
{
	return bucket_pos_to_bp(ca, bucket, 0);
}

static inline struct bpos bucket_pos_to_bp_end(const struct bch_dev *ca, struct bpos bucket)
{
	return bpos_nosnap_predecessor(bucket_pos_to_bp(ca, bpos_nosnap_successor(bucket), 0));
}

static inline enum btree_id backpointer_btree(const struct bch_backpointer *bp)
{

	return BACKPOINTER_STRIPE_PTR(bp)
		? BTREE_ID_stripe_backpointers
		: BTREE_ID_backpointers;
}

static inline enum bch_data_type bch2_bkey_ptr_data_type(struct bkey_s_c k,
							 struct extent_ptr_decoded p,
							 const union bch_extent_entry *entry)
{
	switch (k.k->type) {
	case KEY_TYPE_btree_ptr:
	case KEY_TYPE_btree_ptr_v2:
		return BCH_DATA_btree;
	case KEY_TYPE_extent:
	case KEY_TYPE_reflink_v:
		if (p.has_ec)
			return BCH_DATA_stripe;
		if (p.ptr.cached)
			return BCH_DATA_cached;
		else
			return BCH_DATA_user;
	case KEY_TYPE_stripe: {
		const struct bch_extent_ptr *ptr = &entry->ptr;
		struct bkey_s_c_stripe s = bkey_s_c_to_stripe(k);

		BUG_ON(ptr < s.v->ptrs ||
		       ptr >= s.v->ptrs + s.v->nr_blocks);

		return ptr >= s.v->ptrs + s.v->nr_blocks - s.v->nr_redundant
			? BCH_DATA_parity
			: BCH_DATA_user;
	}
	default:
		BUG();
	}
}

static inline u64 bp_dev_for_ec_removed_dev(u64 ec_idx, unsigned ec_block)
{
	return (ec_idx << 8) | ec_block;
}

static inline struct bpos bch2_extent_ptr_to_bp_pos(const struct bch_fs *c, struct bkey_s_c k,
						    struct extent_ptr_decoded p)
{
	if (k.k->type != KEY_TYPE_stripe) {
		struct bpos pos = POS(p.ptr.dev,
			   ((u64) p.ptr.offset << c->sb.extent_bp_shift) + p.crc.offset);

		if (p.ptr.dev == BCH_SB_MEMBER_INVALID && p.has_ec)
			pos.inode = bp_dev_for_ec_removed_dev(p.ec.idx, p.ec.block);
		return pos;
	} else {
		/*
		 * Put stripe backpointers where they won't collide with the
		 * extent backpointers within the stripe:
		 */
		struct bkey_s_c_stripe s = bkey_s_c_to_stripe(k);
		return POS(p.ptr.dev,
			   ((u64) (p.ptr.offset + le16_to_cpu(s.v->sectors)) <<
			    c->sb.extent_bp_shift) - 1);
	}
}

/*
 * The reconcile phys bits of a backpointer to @k on @dev: @k's pending work,
 * when the pointer is a leaf's, on a rotational device.
 */
static inline enum reconcile_work_id bch2_bp_reconcile_phys(struct bch_fs *c, unsigned level,
							     unsigned dev, struct bkey_s_c k)
{
	return !level && bch2_dev_rotational(c, dev)
		? rb_work_id_phys(bch2_bkey_reconcile_work_id(c, k))
		: RECONCILE_WORK_none;
}

static inline void bch2_extent_ptr_to_bp(struct bch_fs *c,
			   enum btree_id btree_id, unsigned level,
			   struct bkey_s_c k, struct extent_ptr_decoded p,
			   const union bch_extent_entry *entry,
			   struct bkey_i_backpointer *bp)
{
	bkey_backpointer_init(&bp->k_i);
	bp->k.p = bch2_extent_ptr_to_bp_pos(c, k, p);
	bp->v	= (struct bch_backpointer) {
		.btree_id	= btree_id,
		.level		= level,
		.data_type	= bch2_bkey_ptr_data_type(k, p, entry),
		.bucket_gen	= p.ptr.generation,
		.bucket_len	= ptr_disk_sectors(level ? btree_sectors(c) : k.k->size, p),
		.pos		= k.k->p,
	};

	SET_BACKPOINTER_RECONCILE_PHYS(&bp->v, bch2_bp_reconcile_phys(c, level, p.ptr.dev, k));
	SET_BACKPOINTER_ERASURE_CODED(&bp->v, p.has_ec);
	SET_BACKPOINTER_STRIPE_PTR(&bp->v, p.has_ec && p.ptr.dev == BCH_SB_MEMBER_INVALID);
}

/*
 * Whether two backpointers say the same thing, as far as which extent they
 * belong to: everything but the reconcile phys bits. Those are a cache of the
 * extent's pending work, stale after a device's rotational flag flips - that's
 * check_reconcile_work's to repair; it doesn't make a backpointer someone
 * else's.
 */
static inline bool bch2_backpointers_match(struct bch_backpointer l, struct bch_backpointer r)
{
	SET_BACKPOINTER_RECONCILE_PHYS(&l, 0);
	SET_BACKPOINTER_RECONCILE_PHYS(&r, 0);
	return !memcmp(&l, &r, sizeof(l));
}

struct wb_maybe_flush;
#include "alloc/backpointers_gen.h"
#include "alloc/check_backpointers_gen.h"

#define backpointer_scan_for_each(_trans, _bp_iter, _btree, _start, _end,			\
				  _last_flushed, _progress, _bp, _do)				\
({												\
	struct bp_scan_iter _bp_iter __cleanup(bch2_bp_scan_iter_exit);			\
	bch2_bp_scan_iter_init(&_bp_iter, _btree, _start, _progress);				\
	int _ret3 = 0;										\
												\
	while (true) {										\
		u32 _restart_count = bch2_trans_begin(_trans);					\
		_ret3 = 0;									\
												\
		struct bkey_s_c _k =								\
			bch2_bp_scan_iter_peek(_trans, &_bp_iter, _end, _last_flushed);		\
		if (!_k.k)									\
			break;									\
												\
		struct bkey_s_c_backpointer _bp = bkey_s_c_to_backpointer(_k);			\
		_ret3 = bkey_err(_k) ?: (_do);							\
		if (bch2_err_matches(_ret3, BCH_ERR_transaction_restart))			\
			continue;								\
		if (_ret3)									\
			break;									\
												\
		bch2_trans_verify_not_restarted(_trans, _restart_count);			\
		bch2_bp_scan_iter_advance(&_bp_iter);						\
	}											\
												\
	_ret3;											\
})

#endif /* _BCACHEFS_BACKPOINTERS_BACKGROUND_H */
