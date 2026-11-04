/* SPDX-License-Identifier: LGPL-2.1+ */
/* Copyright (C) 2022 Kent Overstreet */

#ifndef _BCACHEFS_PRINTBUF_H
#define _BCACHEFS_PRINTBUF_H

/*
 * Printbufs: Simple strings for printing to, with optional heap allocation
 *
 * This code has provisions for use in userspace, to aid in making other code
 * portable between kernelspace and userspace.
 *
 * Basic example:
 *   struct printbuf buf = PRINTBUF;
 *
 *   prt_printf(&buf, "foo=");
 *   foo_to_text(&buf, foo);
 *   printk("%s", buf.buf);
 *   printbuf_exit(&buf);
 *
 * Or
 *   struct printbuf buf = PRINTBUF_EXTERN(char_buf, char_buf_size)
 *
 * We can now write pretty printers instead of writing code that dumps
 * everything to the kernel log buffer, and then those pretty-printers can be
 * used by other code that outputs to kernel log, sysfs, debugfs, etc.
 *
 * Memory allocation: Outputing to a printbuf may allocate memory. This
 * allocation is done with GFP_KERNEL, by default: use the newer
 * memalloc_*_(save|restore) functions as needed - PF_MEMALLOC_NORECLAIM in
 * atomic context.
 *
 * It's allowed to grab the output buffer and free it later with kfree() instead
 * of using printbuf_exit(), if the user just needs a heap allocated string at
 * the end.
 *
 * Memory allocation failures: We don't return errors directly, because on
 * memory allocation failure we usually don't want to bail out and unwind - we
 * want to print what we've got, on a best-effort basis. But code that does want
 * to return -ENOMEM may check printbuf.allocation_failure.
 *
 * Indenting, tabstops:
 *
 * To aid is writing multi-line pretty printers spread across multiple
 * functions, printbufs track the current indent level.
 *
 * printbuf_indent_push() and printbuf_indent_pop() increase and decrease the current indent
 * level, respectively.
 *
 * To use tabstops, set printbuf->tabstops[]; they are in units of spaces, from
 * start of line. Once set, prt_tab() will output spaces up to the next tabstop.
 * prt_tab_rjust() will also advance the current line of text up to the next
 * tabstop, but it does so by shifting text since the previous tabstop up to the
 * next tabstop - right justifying it.
 *
 * Make sure you use prt_newline() instead of \n in the format string for indent
 * level and tabstops to work corretly.
 *
 * Output units: printbuf->units exists to tell pretty-printers how to output
 * numbers: a raw value (e.g. directly from a superblock field), as bytes, or as
 * human readable bytes. prt_units() obeys it.
 */

#include <linux/hex.h>
#include <linux/kernel.h>
#include <linux/string.h>

#include "enum_kind.h"
#include "util/printbuf_gen.h"

__printf(2, 3) void bch2_prt_printf(struct printbuf *out, const char *fmt, ...);
__printf(2, 0) void bch2_prt_vprintf(struct printbuf *out, const char *fmt, va_list);

/* Initializer for a heap allocated printbuf: */
#define PRINTBUF ((struct printbuf) { .heap_allocated = true })

/* Initializer a printbuf that points to an external buffer: */
#define PRINTBUF_EXTERN(_buf, _size)			\
((struct printbuf) {					\
	.buf	= _buf,					\
	.size	= _size,				\
})

static inline struct printbuf bch2_printbuf_init(void)
{
	return PRINTBUF;
}

DEFINE_CLASS(printbuf, struct printbuf,
	     bch2_printbuf_exit(&_T), bch2_printbuf_init(), void)

/*
 * Returns number of characters we can print to the output buffer - i.e.
 * excluding the terminating nul:
 */
static inline unsigned printbuf_remaining(struct printbuf *out)
{
	return bch2_printbuf_remaining(out);
}

static inline void printbuf_nul_terminate(struct printbuf *out)
{
	bch2_printbuf_nul_terminate(out);
}

static inline void prt_char(struct printbuf *out, char c)
{
	bch2_prt_chars(out, c, 1);
}

static inline void prt_str(struct printbuf *out, const char *str)
{
	bch2_prt_bytes_indented(out, str, strlen(str));
}

static inline void prt_hex_bytes(struct printbuf *out, const void *b, unsigned n)
{
	bch2_prt_hex_bytes(out, b, n);
}

static inline void prt_hex_byte(struct printbuf *out, u8 byte)
{
	bch2_prt_hex_bytes(out, &byte, 1);
}

/**
 * printbuf_reset - re-use a printbuf without freeing and re-initializing it:
 */
static inline void printbuf_reset(struct printbuf *buf)
{
	bch2_printbuf_reset(buf);
}

DEFINE_GUARD(printbuf_indent, struct printbuf *,
	     bch2_printbuf_indent_add(_T, 2),
	     bch2_printbuf_indent_sub(_T, 2));

DEFINE_GUARD(printbuf_indent_nextline, struct printbuf *,
	     bch2_printbuf_indent_add_nextline(_T, 2),
	     bch2_printbuf_indent_sub(_T, 2));

#endif /* _BCACHEFS_PRINTBUF_H */
