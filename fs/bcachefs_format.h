/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_FORMAT_H
#define _BCACHEFS_FORMAT_H

/*
 * bcachefs on disk data structures: the types are defined in Rust, in
 * bcachefs_format_types.rs, which has the overview, and generated into
 * bcachefs_format_gen.h. This is what C has for them besides - its macros and
 * inline functions.
 */

#include "enum_kind.h"

#include <asm/types.h>
#include <asm/byteorder.h>
#include <linux/kernel.h>
#include <linux/uuid.h>
#include <uapi/linux/magic.h>

#include "util/vstructs.h"

#define BITMASK(name, type, field, offset, end)				\
static const __maybe_unused unsigned	name##_OFFSET = offset;		\
static const __maybe_unused unsigned	name##_BITS = (end - offset);	\
									\
static inline __u64 name(const type *k)					\
{									\
	return (k->field >> offset) & ~(~0ULL << (end - offset));	\
}									\
									\
static inline void SET_##name(type *k, __u64 v)				\
{									\
	k->field &= ~(~(~0ULL << (end - offset)) << offset);		\
	k->field |= (v & ~(~0ULL << (end - offset))) << offset;		\
}

#define LE_BITMASK(_bits, name, type, field, offset, end)		\
static const __maybe_unused unsigned	name##_OFFSET = offset;		\
static const __maybe_unused unsigned	name##_BITS = (end - offset);	\
static const __maybe_unused __u##_bits	name##_MAX = (1ULL << (end - offset)) - 1;\
									\
static inline __u64 name(const type *k)					\
{									\
	return (__le##_bits##_to_cpu(k->field) >> offset) &		\
		~(~0ULL << (end - offset));				\
}									\
									\
static inline void SET_##name(type *k, __u64 v)				\
{									\
	__u##_bits new = __le##_bits##_to_cpu(k->field);		\
									\
	new &= ~(~(~0ULL << (end - offset)) << offset);			\
	new |= (v & ~(~0ULL << (end - offset))) << offset;		\
	k->field = __cpu_to_le##_bits(new);				\
}

#define LE16_BITMASK(n, t, f, o, e)	LE_BITMASK(16, n, t, f, o, e)
#define LE32_BITMASK(n, t, f, o, e)	LE_BITMASK(32, n, t, f, o, e)
#define LE64_BITMASK(n, t, f, o, e)	LE_BITMASK(64, n, t, f, o, e)

#ifndef __nonstring
#define __nonstring
#endif

#include "bcachefs_format_gen.h"

#define KEY_INODE_MAX			((__u64)~0ULL)
#define KEY_OFFSET_MAX			((__u64)~0ULL)
#define KEY_SNAPSHOT_MAX		((__u32)~0U)
#define KEY_SIZE_MAX			((__u32)~0U)

#define SPOS(_inode, _offset, _snapshot)		\
	((struct bpos) {				\
		.inode		= _inode,		\
		.offset		= _offset,		\
		.snapshot	= _snapshot,		\
	})

#define POS_MIN				SPOS(0, 0, 0)
#define POS_MAX				SPOS(KEY_INODE_MAX, KEY_OFFSET_MAX, 0)
#define SPOS_MAX			SPOS(KEY_INODE_MAX, KEY_OFFSET_MAX, KEY_SNAPSHOT_MAX)
#define POS(_inode, _offset)		SPOS(_inode, _offset, 0)

#define bkey_format_field(name, field)					\
	[BKEY_FIELD_##name] = (sizeof(((struct bkey *) NULL)->field) * 8)

#define BKEY_FORMAT_CURRENT						\
((struct bkey_format) {							\
	.key_u64s	= BKEY_U64s,					\
	.nr_fields	= BKEY_NR_FIELDS,				\
	.bits_per_field = {						\
		bkey_format_field(INODE,	p.inode),		\
		bkey_format_field(OFFSET,	p.offset),		\
		bkey_format_field(SNAPSHOT,	p.snapshot),		\
		bkey_format_field(SIZE,		size),			\
		bkey_format_field(VERSION_HI,	bversion.hi),		\
		bkey_format_field(VERSION_LO,	bversion.lo),		\
	},								\
})

#define POS_KEY(_pos)							\
((struct bkey) {							\
	.u64s		= BKEY_U64s,					\
	.format		= KEY_FORMAT_CURRENT,				\
	.p		= _pos,						\
})

#define KEY(_inode, _offset, _size)					\
((struct bkey) {							\
	.u64s		= BKEY_U64s,					\
	.format		= KEY_FORMAT_CURRENT,				\
	.p		= POS(_inode, _offset),				\
	.size		= _size,					\
})

#define bkey_bytes(_k)		((_k)->u64s * sizeof(__u64))

#define __BKEY_PADDED(key, pad)					\
	struct bkey_i key; __u64 key ## _pad[pad]

#define BCH_KEY_MAGIC					\
	(((__u64) 'b' <<  0)|((__u64) 'c' <<  8)|		\
	 ((__u64) 'h' << 16)|((__u64) '*' << 24)|		\
	 ((__u64) '*' << 32)|((__u64) 'k' << 40)|		\
	 ((__u64) 'e' << 48)|((__u64) 'y' << 56))


#define BCH_VERSION_MAJOR(_v)		((__u16) ((_v) >> 10))
#define BCH_VERSION_MINOR(_v)		((__u16) ((_v) & ~(~0U << 10)))
#define BCH_VERSION(_major, _minor)	(((_major) << 10)|(_minor) << 0)

static const __maybe_unused
unsigned bcachefs_metadata_required_upgrade_below = bcachefs_metadata_version_btree_node_accounting;

static const __maybe_unused unsigned bch_crc_bytes[] = {
	[BCH_CSUM_none]				= 0,
	[BCH_CSUM_crc32c_nonzero]		= 4,
	[BCH_CSUM_crc32c]			= 4,
	[BCH_CSUM_crc64_nonzero]		= 8,
	[BCH_CSUM_crc64]			= 8,
	[BCH_CSUM_xxhash]			= 8,
	[BCH_CSUM_chacha20_poly1305_80]		= 10,
	[BCH_CSUM_chacha20_poly1305_128]	= 16,
};

/*
 * Magic numbers
 *
 * The various other data structures have their own magic numbers, which are
 * xored with the first part of the cache set's UUID
 */

#define BCACHE_MAGIC							\
	UUID_INIT(0xc68573f6, 0x4e1a, 0x45ca,				\
		  0x82, 0x65, 0xf5, 0x7f, 0x48, 0xba, 0x6d, 0x81)
#define BCHFS_MAGIC							\
	UUID_INIT(0xc68573f6, 0x66ce, 0x90a9,				\
		  0xd9, 0x6a, 0x60, 0xcf, 0x80, 0x3d, 0xf7, 0xef)

#define JSET_MAGIC		__cpu_to_le64(0x245235c1a3625032ULL)
#define BSET_MAGIC		__cpu_to_le64(0x90135c78b99e07f5ULL)

#define replicas_entry_bytes(_i)					\
	(offsetof(typeof(*(_i)), devs) + (_i)->nr_devs)

#define replicas_entry_add_dev(e, d) ({					\
	(e)->nr_devs++;							\
	(e)->devs[(e)->nr_devs - 1] = (d);				\
})

__maybe_unused
static const enum btree_id reconcile_work_btree[] = {
	[RECONCILE_WORK_hipri]		= BTREE_ID_reconcile_hipri,
	[RECONCILE_WORK_normal]		= BTREE_ID_reconcile_work,
	[RECONCILE_WORK_pending]	= BTREE_ID_reconcile_pending,
};

__maybe_unused
static const enum btree_id reconcile_work_phys_btree[] = {
	[RECONCILE_WORK_hipri]		= BTREE_ID_reconcile_hipri_phys,
	[RECONCILE_WORK_normal]		= BTREE_ID_reconcile_work_phys,
};

#define INODEv3_FIELDS_START_CUR	(offsetof(struct bch_inode_v3, fields) / sizeof(__u64))

#define BCH_SB_MEMBER_DELETED_UUID					\
	UUID_INIT(0xffffffff, 0xffff, 0xffff,				\
		  0xd9, 0x6a, 0x60, 0xcf, 0x80, 0x3d, 0xf7, 0xef)

#define SUBVOL_POS_MIN		POS(0, 1)
#define SUBVOL_POS_MAX		POS(0, S32_MAX)

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
