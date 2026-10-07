/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_ALLOC_ACCOUNTING_DEFS_H
#define _BCACHEFS_ALLOC_ACCOUNTING_DEFS_H

#include "enum_kind.h"

enum __enum_closed bch_accounting_mode {
	BCH_ACCOUNTING_normal,
	BCH_ACCOUNTING_gc,
	BCH_ACCOUNTING_read,
};

#endif /* _BCACHEFS_ALLOC_ACCOUNTING_DEFS_H */
