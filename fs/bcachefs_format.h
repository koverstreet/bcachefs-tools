/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_FORMAT_H
#define _BCACHEFS_FORMAT_H

/*
 * bcachefs on disk data structures: the types are bcachefs_format_types.h's;
 * this is what C has for them besides - the inline functions, which a types
 * header can't have, as it's to be defined in Rust.
 */

#include "bcachefs_format_types.h"

static inline void bkey_init(struct bkey *k)
{
	*k = KEY(0, 0, 0);
}

static inline __u64 BCH_SB_COMPRESSION_TYPE(const struct bch_sb *sb)
{
	return BCH_SB_COMPRESSION_TYPE_LO(sb) | (BCH_SB_COMPRESSION_TYPE_HI(sb) << 4);
}

static inline void SET_BCH_SB_COMPRESSION_TYPE(struct bch_sb *sb, __u64 v)
{
	SET_BCH_SB_COMPRESSION_TYPE_LO(sb, v);
	SET_BCH_SB_COMPRESSION_TYPE_HI(sb, v >> 4);
}

static inline __u64 BCH_SB_BACKGROUND_COMPRESSION_TYPE(const struct bch_sb *sb)
{
	return BCH_SB_BACKGROUND_COMPRESSION_TYPE_LO(sb) |
		(BCH_SB_BACKGROUND_COMPRESSION_TYPE_HI(sb) << 4);
}

static inline void SET_BCH_SB_BACKGROUND_COMPRESSION_TYPE(struct bch_sb *sb, __u64 v)
{
	SET_BCH_SB_BACKGROUND_COMPRESSION_TYPE_LO(sb, v);
	SET_BCH_SB_BACKGROUND_COMPRESSION_TYPE_HI(sb, v >> 4);
}

static inline _Bool bch2_csum_type_is_encryption(enum bch_csum_type type)
{
	switch (type) {
	case BCH_CSUM_chacha20_poly1305_80:
	case BCH_CSUM_chacha20_poly1305_128:
		return true;
	default:
		return false;
	}
}

static inline __le64 __bch2_sb_magic(struct bch_sb *sb)
{
	__le64 ret;

	memcpy(&ret, &sb->uuid, sizeof(ret));
	return ret;
}

static inline __u64 __jset_magic(struct bch_sb *sb)
{
	return __le64_to_cpu(__bch2_sb_magic(sb) ^ JSET_MAGIC);
}

static inline __u64 __bset_magic(struct bch_sb *sb)
{
	return __le64_to_cpu(__bch2_sb_magic(sb) ^ BSET_MAGIC);
}

static inline bool jset_entry_is_key(struct jset_entry *e)
{
	switch (e->type) {
	case BCH_JSET_ENTRY_btree_keys:
	case BCH_JSET_ENTRY_btree_root:
	case BCH_JSET_ENTRY_write_buffer_keys:
		return true;
	}

	return false;
}

static inline unsigned jset_entry_dev_usage_nr_types(struct jset_entry_dev_usage *u)
{
	return (vstruct_bytes(&u->entry) - sizeof(struct jset_entry_dev_usage)) /
		sizeof(struct jset_entry_dev_usage_type);
}

static inline unsigned jset_entry_log_msg_bytes(struct jset_entry_log *l)
{
	unsigned b = vstruct_bytes(&l->entry) - offsetof(struct jset_entry_log, d);

	while (b && !l->d[b - 1])
		--b;
	return b;
}

static inline bool btree_id_is_alloc(enum btree_id btree)
{
	switch (btree) {
	case BTREE_ID_alloc:
	case BTREE_ID_backpointers:
	case BTREE_ID_stripe_backpointers:
	case BTREE_ID_need_discard:
	case BTREE_ID_freespace:
	case BTREE_ID_bucket_gens:
	case BTREE_ID_lru:
	case BTREE_ID_accounting:
	case BTREE_ID_reconcile_work:
	case BTREE_ID_reconcile_hipri:
	case BTREE_ID_reconcile_pending:
	case BTREE_ID_reconcile_scan:
		return true;
	default:
		return false;
	}
}

/* We can reconstruct these btrees from information in other btrees */
static inline bool btree_id_can_reconstruct(enum btree_id btree)
{
	if (btree_id_is_alloc(btree))
		return true;

	switch (btree) {
	case BTREE_ID_snapshot_trees:
	case BTREE_ID_deleted_inodes:
	case BTREE_ID_reconcile_work:
	case BTREE_ID_reconcile_hipri:
	case BTREE_ID_reconcile_pending:
	case BTREE_ID_reconcile_scan:
	case BTREE_ID_subvolume_children:
		return true;
	default:
		return false;
	}
}

/*
 * We can reconstruct BTREE_ID_alloc, but reconstucting it from scratch is not
 * so cheap and OOMs on huge filesystems (until we have online
 * check_allocations)
 */
static inline bool btree_id_recovers_from_scan(enum btree_id btree)
{
	return btree == BTREE_ID_alloc || !btree_id_can_reconstruct(btree);
}

static inline __u64 BTREE_NODE_ID(struct btree_node *n)
{
	return BTREE_NODE_ID_LO(n) | (BTREE_NODE_ID_HI(n) << 4);
}

static inline void SET_BTREE_NODE_ID(struct btree_node *n, __u64 v)
{
	SET_BTREE_NODE_ID_LO(n, v);
	SET_BTREE_NODE_ID_HI(n, v >> 4);
}

#endif /* _BCACHEFS_FORMAT_H */
