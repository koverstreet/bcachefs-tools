/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_UTIL_PRINTBUF_TYPES_H
#define _BCACHEFS_UTIL_PRINTBUF_TYPES_H

#include "enum_kind.h"

enum __enum_closed printbuf_si {
	PRINTBUF_UNITS_2,	/* use binary powers of 2^10 */
	PRINTBUF_UNITS_10,	/* use powers of 10^3 (standard SI) */
};

#define PRINTBUF_INLINE_TABSTOPS	8

struct printbuf {
	char			*buf;
	unsigned		size;
	unsigned		pos;
	unsigned		last_newline;
	unsigned		last_field;
	unsigned		indent;
	/*
	 * If nonzero, allocations will be done with GFP_ATOMIC:
	 */
	u8			atomic;
	bool			allocation_failure:1;
	bool			heap_allocated:1;
	bool			overflow:1;
	bool			suppress:1; /* Ratelimited or already printed */
	enum printbuf_si	si_units:1;
	bool			human_readable_units:1;
	bool			has_indent_or_tabstops:1;
	bool			may_vmalloc:1;
	u8			nr_tabstops;

	/*
	 * Do not modify directly: use printbuf_tabstop_add(),
	 * printbuf_tabstop_get()
	 */
	u8			cur_tabstop;
	u8			_tabstops[PRINTBUF_INLINE_TABSTOPS];
};

struct printbuf_restore {
	unsigned		pos;
	unsigned		last_newline;
	unsigned		last_field;
	unsigned		indent;
	u8			cur_tabstop;
};

#endif /* _BCACHEFS_UTIL_PRINTBUF_TYPES_H */
