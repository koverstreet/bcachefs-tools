/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_INIT_PASSES_FORMAT_INLINE_H
#define _BCACHEFS_INIT_PASSES_FORMAT_INLINE_H

/* Included at the end of init/passes_format.h. */

static inline unsigned
recovery_passes_nr_entries(struct bch_sb_field_recovery_passes *r)
{
	return r
		? ((vstruct_end(&r->field) - (void *) &r->start[0]) /
		   sizeof(struct recovery_pass_entry))
		: 0;
}

#endif /* _BCACHEFS_INIT_PASSES_FORMAT_INLINE_H */
