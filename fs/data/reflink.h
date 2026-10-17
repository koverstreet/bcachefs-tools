/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_REFLINK_H
#define _BCACHEFS_REFLINK_H

#include "data/reflink_gen.h"

#define bch2_bkey_ops_reflink_p ((struct bkey_ops) {		\
	.key_validate	= bch2_reflink_p_validate,		\
	.val_to_text	= bch2_reflink_p_to_text,		\
	.key_merge	= bch2_reflink_p_merge,			\
	.trigger	= bch2_trigger_reflink_p,		\
	.check_repair	= bch2_reflink_p_check_repair,		\
})

#define bch2_bkey_ops_reflink_v ((struct bkey_ops) {		\
	.key_validate	= bch2_reflink_v_validate,		\
	.val_to_text	= bch2_reflink_v_to_text,		\
	.swab		= bch2_ptr_swab,			\
	.trigger	= bch2_trigger_reflink_v,		\
	.check_repair	= bch2_check_fix_ptrs,			\
})

#define bch2_bkey_ops_indirect_inline_data ((struct bkey_ops) {	\
	.key_validate	= bch2_indirect_inline_data_validate,	\
	.val_to_text	= bch2_indirect_inline_data_to_text,	\
	.trigger	= bch2_trigger_indirect_inline_data,	\
})

static inline bool bkey_is_indirect(const struct bkey *k)
{
	return  k->type == KEY_TYPE_reflink_v ||
		k->type == KEY_TYPE_indirect_inline_data;
}

#endif /* _BCACHEFS_REFLINK_H */
