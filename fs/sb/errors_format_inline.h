/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_SB_ERRORS_FORMAT_INLINE_H
#define _BCACHEFS_SB_ERRORS_FORMAT_INLINE_H

/* Included at the end of sb/errors_format.h. */

static inline __u64 BCH_SB_ERROR_ENTRY_V2_ID(const struct bch_sb_field_error_entry_v2 *e)
{
	return __le64_to_cpu(e->v[0]) & 0xffff;
}

static inline void SET_BCH_SB_ERROR_ENTRY_V2_ID(struct bch_sb_field_error_entry_v2 *e, __u64 id)
{
	e->v[0] = __cpu_to_le64((__le64_to_cpu(e->v[0]) & ~0xffffULL) |
				(id & 0xffff));
}

static inline __u64 BCH_SB_ERROR_ENTRY_V2_NR(const struct bch_sb_field_error_entry_v2 *e)
{
	return (__le64_to_cpu(e->v[0]) >> 16) & 0xffffffffULL;
}

static inline void SET_BCH_SB_ERROR_ENTRY_V2_NR(struct bch_sb_field_error_entry_v2 *e, __u64 nr)
{
	if (nr > BCH_SB_ERROR_ENTRY_V2_NR_MAX)
		nr = BCH_SB_ERROR_ENTRY_V2_NR_MAX;
	e->v[0] = __cpu_to_le64((__le64_to_cpu(e->v[0]) & ~(0xffffffffULL << 16)) |
				(nr << 16));
}

static inline __u64 BCH_SB_ERROR_ENTRY_V2_LAST(const struct bch_sb_field_error_entry_v2 *e)
{
	return (__le64_to_cpu(e->v[0]) >> 48) |
	       ((__le64_to_cpu(e->v[1]) & 0xffffffULL) << 16);
}

static inline void SET_BCH_SB_ERROR_ENTRY_V2_LAST(struct bch_sb_field_error_entry_v2 *e, __u64 t)
{
	if (t > BCH_SB_ERROR_ENTRY_V2_TIME_MAX)
		t = BCH_SB_ERROR_ENTRY_V2_TIME_MAX;
	e->v[0] = __cpu_to_le64((__le64_to_cpu(e->v[0]) & ~(0xffffULL << 48)) |
				((t & 0xffff) << 48));
	e->v[1] = __cpu_to_le64((__le64_to_cpu(e->v[1]) & ~0xffffffULL) |
				(t >> 16));
}

static inline __u64 BCH_SB_ERROR_ENTRY_V2_FIRST(const struct bch_sb_field_error_entry_v2 *e)
{
	return __le64_to_cpu(e->v[1]) >> 24;
}

static inline void SET_BCH_SB_ERROR_ENTRY_V2_FIRST(struct bch_sb_field_error_entry_v2 *e, __u64 t)
{
	if (t > BCH_SB_ERROR_ENTRY_V2_TIME_MAX)
		t = BCH_SB_ERROR_ENTRY_V2_TIME_MAX;
	e->v[1] = __cpu_to_le64((__le64_to_cpu(e->v[1]) & 0xffffffULL) |
				(t << 24));
}

#endif /* _BCACHEFS_SB_ERRORS_FORMAT_INLINE_H */
