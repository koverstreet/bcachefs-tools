// SPDX-License-Identifier: LGPL-2.1+
/* Copyright (C) 2022 Kent Overstreet */

#include <linux/kernel.h>

#include "printbuf.h"

/*
 * Returns size remaining of output buffer:
 */
static unsigned printbuf_remaining_size(struct printbuf *out)
{
	return out->size ? printbuf_remaining(out) + 1 : 0;
}

static void printbuf_advance_pos(struct printbuf *out, unsigned len)
{
	out->pos += min(len, printbuf_remaining(out));
}

void bch2_prt_vprintf(struct printbuf *out, const char *fmt, va_list args)
{
	int len;

	do {
		va_list args2;

		va_copy(args2, args);
		len = vsnprintf(out->buf + out->pos, printbuf_remaining_size(out), fmt, args2);
		va_end(args2);
	} while (len > printbuf_remaining(out) &&
		 !bch2_printbuf_make_room(out, len));

	unsigned indent_pos = out->pos;
	printbuf_advance_pos(out, len);
	bch2_printbuf_do_indent(out, indent_pos);
}

void bch2_prt_printf(struct printbuf *out, const char *fmt, ...)
{
	va_list args;
	int len;

	do {
		va_start(args, fmt);
		len = vsnprintf(out->buf + out->pos, printbuf_remaining_size(out), fmt, args);
		va_end(args);
	} while (len > printbuf_remaining(out) &&
		 !bch2_printbuf_make_room(out, len));

	unsigned indent_pos = out->pos;
	printbuf_advance_pos(out, len);
	bch2_printbuf_do_indent(out, indent_pos);
}
