/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_JOURNAL_JOURNAL_TYPES_H
#define _BCACHEFS_JOURNAL_JOURNAL_TYPES_H

#include "enum_kind.h"

struct bch_fs;

enum __enum_flags journal_cycle_flags {
	JOURNAL_CYCLE_must_close	= BIT(0),
	JOURNAL_CYCLE_must_open		= BIT(1),
	JOURNAL_CYCLE_force_close	= BIT(2),
};

/* First bits for BCH_WATERMARK: */
enum __enum_closed journal_res_flags {
	__JOURNAL_RES_GET_NONBLOCK	= BCH_WATERMARK_BITS,
	__JOURNAL_RES_GET_CHECK,
};

struct bch_dev;

struct journal_block { struct journal *j; };

#endif /* _BCACHEFS_JOURNAL_JOURNAL_TYPES_H */
