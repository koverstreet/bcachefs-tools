/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_BTREE_BKEY_DEFS_H
#define _BCACHEFS_BTREE_BKEY_DEFS_H

#include "enum_kind.h"

enum __enum_closed bkey_lr_packed {
	BKEY_PACKED_BOTH,
	BKEY_PACKED_RIGHT,
	BKEY_PACKED_LEFT,
	BKEY_PACKED_NONE,
};

#define bkey_lr_packed(_l, _r)						\
	((_l)->format + ((_r)->format << 1))

/*
 * Wrapper for stack-allocated struct bkey_packed.
 *
 * The byte-aligned fast-path unpackers (__bch2_bkey_unpack_key_b,
 * __bkey_unpack_pos_b) issue an 8-byte unaligned load at @bytes +
 * uf->byte_offset, where byte_offset is signed and can be as low as -7
 * (and __bch2_bkey_unpack_key_b's header trick reads @bytes - 1). This
 * is safe for bkeys inside a bset (preceded by the bset header or by
 * another bkey), but reads into the stack redzone for a bare stack-local
 * struct bkey_packed. Wrap stack copies in this to provide the leading
 * padding.
 */
struct bkey_packed_padded {
	u8			_pad[8];
	struct bkey_packed	k;
};

/* Same trick for stack-local struct bkey_i (when used as pack target). */
struct bkey_i_padded {
	u8			_pad[8];
	struct bkey_i		k;
};

struct btree;

#ifdef CONFIG_BCACHEFS_DEBUG

#else

#define bkey_packed(_k)		((_k)->format != KEY_FORMAT_CURRENT)

#endif

enum __enum_closed bkey_pack_pos_ret {
	BKEY_PACK_POS_EXACT,
	BKEY_PACK_POS_SMALLER,
	BKEY_PACK_POS_FAIL,
};

typedef void (*compiled_unpack_fn)(struct bkey *, const struct bkey_packed *);

#define bkey_fields()							\
	x(BKEY_FIELD_INODE,		p.inode)			\
	x(BKEY_FIELD_OFFSET,		p.offset)			\
	x(BKEY_FIELD_SNAPSHOT,		p.snapshot)			\
	x(BKEY_FIELD_SIZE,		size)				\
	x(BKEY_FIELD_VERSION_HI,	bversion.hi)			\
	x(BKEY_FIELD_VERSION_LO,	bversion.lo)

struct bkey_format_state {
	u64 field_min[BKEY_NR_FIELDS];
	u64 field_max[BKEY_NR_FIELDS];
};

#endif /* _BCACHEFS_BTREE_BKEY_DEFS_H */
