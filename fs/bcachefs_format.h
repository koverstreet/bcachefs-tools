/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_FORMAT_H
#define _BCACHEFS_FORMAT_H

/*
 * bcachefs on disk data structures: the types are defined in Rust, in
 * format.rs, which has the overview, and generated into format_gen.h. This
 * is what that's written with: the macros its bitfield accessors and
 * version numbers expand.
 */

#include "enum_kind.h"

#include <asm/types.h>
#include <asm/byteorder.h>
#include <linux/kernel.h>
#include <linux/uuid.h>
#include <uapi/linux/magic.h>

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

#define BCH_VERSION_MAJOR(_v)		((__u16) ((_v) >> 10))
#define BCH_VERSION_MINOR(_v)		((__u16) ((_v) & ~(~0U << 10)))
#define BCH_VERSION(_major, _minor)	(((_major) << 10)|(_minor) << 0)

#include "format_gen.h"

#endif /* _BCACHEFS_FORMAT_H */
