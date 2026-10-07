/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_ALLOC_ACCOUNTING_FORMAT_INLINE_H
#define _BCACHEFS_ALLOC_ACCOUNTING_FORMAT_INLINE_H

/* Included at the end of alloc/accounting_format.h. */

static inline bool data_type_is_empty(enum bch_data_type type)
{
	switch (type) {
	case BCH_DATA_free:
	case BCH_DATA_need_gc_gens:
	case BCH_DATA_need_discard:
		return true;
	default:
		return false;
	}
}

static inline bool data_type_is_hidden(enum bch_data_type type)
{
	switch (type) {
	case BCH_DATA_sb:
	case BCH_DATA_journal:
		return true;
	default:
		return false;
	}
}

#endif /* _BCACHEFS_ALLOC_ACCOUNTING_FORMAT_INLINE_H */
