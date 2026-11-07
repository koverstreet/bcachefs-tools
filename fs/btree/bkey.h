/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_BKEY_H
#define _BCACHEFS_BKEY_H

#include <linux/bug.h>
#include <linux/static_key.h>
#include "bcachefs_format.h"
#include "btree/bkey_gen.h"
#include "btree/node/bset_gen.h"
#include "btree/node/iter_gen.h"
#include "btree/path_gen.h"
#include "btree/types_gen.h"
#include "btree/node_gen.h"
#include "util/util.h"
#include "util/vstructs.h"

#if 0

/*
 * compiled unpack functions are disabled, pending a new interface for
 * dynamically allocating executable memory:
 */

#ifdef CONFIG_X86_64
#define HAVE_BCACHEFS_COMPILED_UNPACK	1
#endif
#endif

#include "btree/bkey_defs_gen.h"

static inline void bkey_p_copy(struct bkey_packed *dst, const struct bkey_packed *src)
{
	memcpy_u64s_small(dst, src, src->u64s);
}

static inline void bkey_copy(struct bkey_i *dst, const struct bkey_i *src)
{
	memcpy_u64s(dst, src, src->k.u64s);
}

static inline void bkey_val_copy(struct bkey_i *dst, const struct bkey_i *src)
{
	memcpy_u64s(&dst->v, &src->v, bkey_val_u64s(&src->k));
	dst->k.type	= src->k.type;
	dst->k.u64s	= src->k.u64s;
}

extern struct static_key_false bch2_debug_check_bkey_unpack;

static inline __pure
int bkey_cmp_left_packed(const struct btree *b,
			 const struct bkey_packed *l, const struct bpos *r)
{
	return __bch2_bkey_cmp_left_packed(b, l, r);
}

/*
 * The compiler generates better code when we pass bpos by ref, but it's often
 * enough terribly convenient to pass it by val... as much as I hate c++, const
 * ref would be nice here:
 */
__pure __flatten
static inline int bkey_cmp_left_packed_byval(const struct btree *b,
					     const struct bkey_packed *l,
					     struct bpos r)
{
	return bkey_cmp_left_packed(b, l, &r);
}

static __always_inline bool bpos_eq(struct bpos l, struct bpos r)
{
	return  !((l.inode	^ r.inode) |
		  (l.offset	^ r.offset) |
		  (l.snapshot	^ r.snapshot));
}

static __always_inline bool bpos_lt(struct bpos l, struct bpos r)
{
	return  l.inode	!= r.inode ? l.inode < r.inode :
		l.offset != r.offset ? l.offset < r.offset :
		l.snapshot != r.snapshot ? l.snapshot < r.snapshot : false;
}

static __always_inline bool bpos_le(struct bpos l, struct bpos r)
{
	return  l.inode	!= r.inode ? l.inode < r.inode :
		l.offset != r.offset ? l.offset < r.offset :
		l.snapshot != r.snapshot ? l.snapshot < r.snapshot : true;
}

static __always_inline bool bpos_gt(struct bpos l, struct bpos r)
{
	return bpos_lt(r, l);
}

static __always_inline bool bpos_ge(struct bpos l, struct bpos r)
{
	return bpos_le(r, l);
}

static __always_inline int bpos_cmp(struct bpos l, struct bpos r)
{
	return  cmp_int(l.inode,    r.inode) ?:
		cmp_int(l.offset,   r.offset) ?:
		cmp_int(l.snapshot, r.snapshot);
}

static inline struct bpos bpos_min(struct bpos l, struct bpos r)
{
	return bpos_lt(l, r) ? l : r;
}

static inline struct bpos bpos_max(struct bpos l, struct bpos r)
{
	return bpos_gt(l, r) ? l : r;
}

static __always_inline bool bkey_eq(struct bpos l, struct bpos r)
{
	return  !((l.inode	^ r.inode) |
		  (l.offset	^ r.offset));
}

static __always_inline bool bkey_lt(struct bpos l, struct bpos r)
{
	return  l.inode	!= r.inode
		? l.inode < r.inode
		: l.offset < r.offset;
}

static __always_inline bool bkey_le(struct bpos l, struct bpos r)
{
	return  l.inode	!= r.inode
		? l.inode < r.inode
		: l.offset <= r.offset;
}

static __always_inline bool bkey_gt(struct bpos l, struct bpos r)
{
	return bkey_lt(r, l);
}

static __always_inline bool bkey_ge(struct bpos l, struct bpos r)
{
	return bkey_le(r, l);
}

static __always_inline int bkey_cmp(struct bpos l, struct bpos r)
{
	return  cmp_int(l.inode,    r.inode) ?:
		cmp_int(l.offset,   r.offset);
}

static inline struct bpos bkey_min(struct bpos l, struct bpos r)
{
	return bkey_lt(l, r) ? l : r;
}

static inline struct bpos bkey_max(struct bpos l, struct bpos r)
{
	return bkey_gt(l, r) ? l : r;
}

static __always_inline int bversion_cmp(struct bversion l, struct bversion r)
{
	return  cmp_int(l.hi, r.hi) ?:
		cmp_int(l.lo, r.lo);
}

static __always_inline bool bversion_eq(struct bversion l, struct bversion r)
{
	return  l.hi == r.hi &&
		l.lo == r.lo;
}

static inline bool bkey_fields_eq(const struct bkey *l, const struct bkey *r)
{
	return (l->u64s == r->u64s &&
		l->type == r->type &&
		bpos_eq(l->p, r->p) &&
		bversion_eq(l->bversion, r->bversion) &&
		l->size == r->size);
}

static inline bool bkey_and_val_eq(struct bkey_s_c l, struct bkey_s_c r)
{
	return bkey_fields_eq(l.k, r.k) &&
		!memcmp(l.v, r.v, bkey_val_bytes(l.k));
}

#define ZERO_VERSION	((struct bversion) { .hi = 0, .lo = 0 })
#define MAX_VERSION	((struct bversion) { .hi = ~0, .lo = ~0ULL })

static __always_inline bool bversion_zero(struct bversion v)
{
	return bversion_cmp(v, ZERO_VERSION) == 0;
}

#ifdef CONFIG_BCACHEFS_DEBUG
/* statement expressions confusing unlikely()? */
#define bkey_packed(_k)							\
	({ EBUG_ON((_k)->format > KEY_FORMAT_CURRENT);			\
	 (_k)->format != KEY_FORMAT_CURRENT; })
#else
#define bkey_packed(_k)		((_k)->format != KEY_FORMAT_CURRENT)
#endif

/*
 * It's safe to treat an unpacked bkey as a packed one, but not the reverse
 */
static inline struct bkey_packed *bkey_to_packed(struct bkey_i *k)
{
	return (struct bkey_packed *) k;
}

static inline const struct bkey_packed *bkey_to_packed_c(const struct bkey_i *k)
{
	return (const struct bkey_packed *) k;
}

static inline struct bkey_i *packed_to_bkey(struct bkey_packed *k)
{
	return bkey_packed(k) ? NULL : (struct bkey_i *) k;
}

static inline const struct bkey *packed_to_bkey_c(const struct bkey_packed *k)
{
	return bkey_packed(k) ? NULL : (const struct bkey *) k;
}

static inline unsigned bkey_format_key_bits(const struct bkey_format *format)
{
	return format->bits_per_field[BKEY_FIELD_INODE] +
		format->bits_per_field[BKEY_FIELD_OFFSET] +
		format->bits_per_field[BKEY_FIELD_SNAPSHOT];
}

static inline struct bpos bpos_successor(struct bpos p)
{
	if (!++p.snapshot &&
	    !++p.offset &&
	    !++p.inode)
		BUG();

	return p;
}

static inline struct bpos bpos_predecessor(struct bpos p)
{
	if (!p.snapshot-- &&
	    !p.offset-- &&
	    !p.inode--)
		BUG();

	return p;
}

static inline struct bpos bpos_nosnap_successor(struct bpos p)
{
	p.snapshot = 0;

	if (!++p.offset &&
	    !++p.inode)
		BUG();

	return p;
}

static inline struct bpos bpos_nosnap_predecessor(struct bpos p)
{
	p.snapshot = 0;

	if (!p.offset-- &&
	    !p.inode--)
		BUG();

	return p;
}

static inline u64 bkey_start_offset(const struct bkey *k)
{
	return k->p.offset - k->size;
}

static inline struct bpos bkey_start_pos(const struct bkey *k)
{
	return (struct bpos) {
		.inode		= k->p.inode,
		.offset		= bkey_start_offset(k),
		.snapshot	= k->p.snapshot,
	};
}

static inline struct bpos bpos_with_snapshot(struct bpos p, u32 snapshot)
{
	p.snapshot = snapshot;
	return p;
}

/* Packed helpers */

static inline unsigned bkeyp_key_u64s(const struct bkey_format *format,
				      const struct bkey_packed *k)
{
	return bkey_packed(k) ? format->key_u64s : BKEY_U64s;
}

static inline unsigned bkeyp_key_bytes(const struct bkey_format *format,
				       const struct bkey_packed *k)
{
	return bkeyp_key_u64s(format, k) * sizeof(u64);
}

static inline unsigned bkeyp_val_u64s(const struct bkey_format *format,
				      const struct bkey_packed *k)
{
	return k->u64s - bkeyp_key_u64s(format, k);
}

static inline size_t bkeyp_val_bytes(const struct bkey_format *format,
				     const struct bkey_packed *k)
{
	return bkeyp_val_u64s(format, k) * sizeof(u64);
}

static inline void set_bkeyp_val_u64s(const struct bkey_format *format,
				      struct bkey_packed *k, unsigned val_u64s)
{
	k->u64s = bkeyp_key_u64s(format, k) + val_u64s;
}

#define bkeyp_val(_format, _k)						\
	 ((struct bch_val *) ((u64 *) (_k)->_data + bkeyp_key_u64s(_format, _k)))

static inline void
__bkey_unpack_key_format_checked(const struct btree *b,
			       struct bkey *dst,
			       const struct bkey_packed *src)
{
	if (IS_ENABLED(HAVE_BCACHEFS_COMPILED_UNPACK)) {
		compiled_unpack_fn unpack_fn = b->aux_data;
		unpack_fn(dst, src);

		if (IS_ENABLED(CONFIG_BCACHEFS_DEBUG) &&
		    static_branch_unlikely(&bch2_debug_check_bkey_unpack)) {
			struct bkey dst2;

			__bch2_bkey_unpack_key(&b->format, &dst2, src);
			BUG_ON(memcmp(dst, &dst2, sizeof(*dst)));
		}
	} else {
		__bch2_bkey_unpack_key_b(b, dst, src);

		if (IS_ENABLED(CONFIG_BCACHEFS_DEBUG) &&
		    static_branch_unlikely(&bch2_debug_check_bkey_unpack)) {
			struct bkey dst2;

			__bch2_bkey_unpack_key(&b->format, &dst2, src);
			BUG_ON(memcmp(dst, &dst2, sizeof(*dst)));
		}
	}
}

static inline struct bkey
bkey_unpack_key_format_checked(const struct btree *b,
			       const struct bkey_packed *src)
{
	struct bkey dst;

	__bkey_unpack_key_format_checked(b, &dst, src);
	return dst;
}

static inline void __bkey_unpack_key(const struct btree *b,
				     struct bkey *dst,
				     const struct bkey_packed *src)
{
	if (likely(bkey_packed(src)))
		__bkey_unpack_key_format_checked(b, dst, src);
	else
		*dst = *packed_to_bkey_c(src);
}

/**
 * bkey_unpack_key -- unpack just the key, not the value
 */
static inline struct bkey bkey_unpack_key(const struct btree *b,
					  const struct bkey_packed *src)
{
	return likely(bkey_packed(src))
		? bkey_unpack_key_format_checked(b, src)
		: *packed_to_bkey_c(src);
}

static inline struct bpos
bkey_unpack_pos_format_checked(const struct btree *b,
			       const struct bkey_packed *src)
{
#ifdef HAVE_BCACHEFS_COMPILED_UNPACK
	return bkey_unpack_key_format_checked(b, src).p;
#else
	return __bkey_unpack_pos_b(b, src);
#endif
}

static inline struct bpos bkey_unpack_pos(const struct btree *b,
					  const struct bkey_packed *src)
{
	return likely(bkey_packed(src))
		? bkey_unpack_pos_format_checked(b, src)
		: packed_to_bkey_c(src)->p;
}

/* Disassembled bkeys */

static inline struct bkey_s_c bkey_disassemble(const struct btree *b,
					       const struct bkey_packed *k,
					       struct bkey *u)
{
	__bkey_unpack_key(b, u, k);

	return (struct bkey_s_c) { u, bkeyp_val(&b->format, k), };
}

static inline void bkey_reassemble(struct bkey_i *dst,
				   struct bkey_s_c src)
{
	dst->k = *src.k;
	/*
	 * @src may have been unpacked by a path that doesn't write every
	 * byte of the unpacked key - don't leak uninitialized memory when
	 * the result is copied to userspace (BCH_IOCTL_QUERY_BTREE_KEYS):
	 */
	memset(dst->k.pad, 0, sizeof(dst->k.pad));
	memcpy_u64s(&dst->v, src.v, bkey_val_u64s(src.k));
}

#endif /* _BCACHEFS_BKEY_H */
