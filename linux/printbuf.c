// SPDX-License-Identifier: LGPL-2.1+
/* Copyright (C) 2022 Kent Overstreet */

#include <linux/err.h>
#include <linux/export.h>
#include <linux/kernel.h>
#include <linux/printbuf.h>
#include <linux/slab.h>
#include <linux/string_helpers.h>

static inline unsigned printbuf_linelen(struct printbuf *buf)
{
	return buf->pos - buf->last_newline;
}

int printbuf_make_room(struct printbuf *out, unsigned extra)
{
	unsigned new_size;
	char *buf;

	if (!out->heap_allocated)
		return 0;

	/* Reserved space for terminating nul: */
	extra += 1;

	if (out->pos + extra < out->size)
		return 0;

	new_size = roundup_pow_of_two(out->size + extra);

	/*
	 * Note: output buffer must be freeable with kfree(), it's not required
	 * that the user use printbuf_exit().
	 */
	buf = krealloc(out->buf, new_size, !out->atomic ? GFP_KERNEL : GFP_NOWAIT);

	if (!buf) {
		out->allocation_failure = true;
		return -ENOMEM;
	}

	out->buf	= buf;
	out->size	= new_size;
	return 0;
}
EXPORT_SYMBOL(printbuf_make_room);

/**
 * printbuf_str - returns printbuf's buf as a C string, guaranteed to be null
 * terminated
 */
const char *printbuf_str(const struct printbuf *buf)
{
	/*
	 * If we've written to a printbuf then it's guaranteed to be a null
	 * terminated string - but if we haven't, then we might not have
	 * allocated a buffer at all:
	 */
	return buf->pos
		? buf->buf
		: "";
}
EXPORT_SYMBOL(printbuf_str);

/**
 * printbuf_exit - exit a printbuf, freeing memory it owns and poisoning it
 * against accidental use.
 */
void printbuf_exit(struct printbuf *buf)
{
	if (buf->heap_allocated) {
		kfree(buf->buf);
		buf->buf = ERR_PTR(-EINTR); /* poison value */
	}
}
EXPORT_SYMBOL(printbuf_exit);

void printbuf_tabstops_reset(struct printbuf *buf)
{
	buf->nr_tabstops = 0;
}
EXPORT_SYMBOL(printbuf_tabstops_reset);

void printbuf_tabstop_pop(struct printbuf *buf)
{
	if (buf->nr_tabstops)
		--buf->nr_tabstops;
}
EXPORT_SYMBOL(printbuf_tabstop_pop);

/*
 * printbuf_tabstop_set - add a tabstop, n spaces from the previous tabstop
 *
 * @buf: printbuf to control
 * @spaces: number of spaces from previous tabpstop
 *
 * In the future this function may allocate memory if setting more than
 * PRINTBUF_INLINE_TABSTOPS or setting tabstops more than 255 spaces from start
 * of line.
 */
int printbuf_tabstop_push(struct printbuf *buf, unsigned spaces)
{
	unsigned prev_tabstop = buf->nr_tabstops
		? buf->_tabstops[buf->nr_tabstops - 1]
		: 0;

	if (WARN_ON(buf->nr_tabstops >= ARRAY_SIZE(buf->_tabstops)))
		return -EINVAL;

	buf->_tabstops[buf->nr_tabstops++] = prev_tabstop + spaces;
	buf->has_indent_or_tabstops = true;
	return 0;
}
EXPORT_SYMBOL(printbuf_tabstop_push);

/**
 * printbuf_indent_add - add to the current indent level
 *
 * @buf: printbuf to control
 * @spaces: number of spaces to add to the current indent level
 *
 * Subsequent lines, and the current line if the output position is at the start
 * of the current line, will be indented by @spaces more spaces.
 */
void printbuf_indent_add(struct printbuf *buf, unsigned spaces)
{
	if (WARN_ON_ONCE(buf->indent + spaces < buf->indent))
		spaces = 0;

	buf->indent += spaces;
	prt_chars(buf, ' ', spaces);

	buf->has_indent_or_tabstops = true;
}
EXPORT_SYMBOL(printbuf_indent_add);

/**
 * printbuf_indent_sub - subtract from the current indent level
 *
 * @buf: printbuf to control
 * @spaces: number of spaces to subtract from the current indent level
 *
 * Subsequent lines, and the current line if the output position is at the start
 * of the current line, will be indented by @spaces less spaces.
 */
void printbuf_indent_sub(struct printbuf *buf, unsigned spaces)
{
	if (WARN_ON_ONCE(spaces > buf->indent))
		spaces = buf->indent;

	if (buf->last_newline + buf->indent == buf->pos) {
		buf->pos -= spaces;
		printbuf_nul_terminate(buf);
	}
	buf->indent -= spaces;

	if (!buf->indent && !buf->nr_tabstops)
		buf->has_indent_or_tabstops = false;
}
EXPORT_SYMBOL(printbuf_indent_sub);

void prt_newline(struct printbuf *buf)
{
	unsigned i;

	printbuf_make_room(buf, 1 + buf->indent);

	__prt_char(buf, '\n');

	buf->last_newline	= buf->pos;

	for (i = 0; i < buf->indent; i++)
		__prt_char(buf, ' ');

	printbuf_nul_terminate(buf);

	buf->last_field		= buf->pos;
	buf->cur_tabstop	= 0;
}
EXPORT_SYMBOL(prt_newline);

/*
 * Returns spaces from start of line, if set, or 0 if unset:
 */
static inline unsigned cur_tabstop(struct printbuf *buf)
{
	return buf->cur_tabstop < buf->nr_tabstops
		? buf->_tabstops[buf->cur_tabstop]
		: 0;
}

static void __prt_tab(struct printbuf *out)
{
	int spaces = max_t(int, 0, cur_tabstop(out) - printbuf_linelen(out));

	prt_chars(out, ' ', spaces);

	out->last_field = out->pos;
	out->cur_tabstop++;
}

/**
 * prt_tab - Advance printbuf to the next tabstop
 *
 * @buf: printbuf to control
 *
 * Advance output to the next tabstop by printing spaces.
 */
void prt_tab(struct printbuf *out)
{
	if (WARN_ON(!cur_tabstop(out)))
		return;

	__prt_tab(out);
}
EXPORT_SYMBOL(prt_tab);

static void __prt_tab_rjust(struct printbuf *buf)
{
	unsigned move = buf->pos - buf->last_field;
	int pad = (int) cur_tabstop(buf) - (int) printbuf_linelen(buf);

	if (pad > 0) {
		printbuf_make_room(buf, pad);

		if (buf->last_field + pad < buf->size)
			memmove(buf->buf + buf->last_field + pad,
				buf->buf + buf->last_field,
				min(move, buf->size - 1 - buf->last_field - pad));

		if (buf->last_field < buf->size)
			memset(buf->buf + buf->last_field, ' ',
			       min((unsigned) pad, buf->size - buf->last_field));

		buf->pos += pad;
		printbuf_nul_terminate(buf);
	}

	buf->last_field = buf->pos;
	buf->cur_tabstop++;
}

/**
 * prt_tab_rjust - Advance printbuf to the next tabstop, right justifying
 * previous output
 *
 * @buf: printbuf to control
 *
 * Advance output to the next tabstop by inserting spaces immediately after the
 * previous tabstop, right justifying previously outputted text.
 */
void prt_tab_rjust(struct printbuf *buf)
{
	if (WARN_ON(!cur_tabstop(buf)))
		return;

	__prt_tab_rjust(buf);
}
EXPORT_SYMBOL(prt_tab_rjust);

/**
 * prt_bytes_indented - Print an array of chars, handling embedded control characters
 *
 * @out: printbuf to output to
 * @str: string to print
 * @count: number of bytes to print
 *
 * The following control characters are handled as so:
 *   \n: prt_newline	newline that obeys current indent level
 *   \t: prt_tab	advance to next tabstop
 *   \r: prt_tab_rjust	advance to next tabstop, with right justification
 */
void prt_bytes_indented(struct printbuf *out, const char *str, unsigned count)
{
	const char *unprinted_start = str;
	const char *end = str + count;

	if (!out->has_indent_or_tabstops || out->suppress_indent_tabstop_handling) {
		prt_bytes(out, str, count);
		return;
	}

	while (str != end) {
		switch (*str) {
		case '\n':
			prt_bytes(out, unprinted_start, str - unprinted_start);
			unprinted_start = str + 1;
			prt_newline(out);
			break;
		case '\t':
			if (likely(cur_tabstop(out))) {
				prt_bytes(out, unprinted_start, str - unprinted_start);
				unprinted_start = str + 1;
				__prt_tab(out);
			}
			break;
		case '\r':
			if (likely(cur_tabstop(out))) {
				prt_bytes(out, unprinted_start, str - unprinted_start);
				unprinted_start = str + 1;
				__prt_tab_rjust(out);
			}
			break;
		}

		str++;
	}

	prt_bytes(out, unprinted_start, str - unprinted_start);
}
EXPORT_SYMBOL(prt_bytes_indented);

/**
 * prt_human_readable_u64 - Print out a u64 in human readable units
 *
 * Units of 2^10 (default) or 10^3 are controlled via @buf->si_units
 */
void prt_human_readable_u64(struct printbuf *buf, u64 v)
{
	printbuf_make_room(buf, 10);
	buf->pos += string_get_size(v, 1, !buf->si_units,
				    buf->buf + buf->pos,
				    printbuf_remaining_size(buf));
}
EXPORT_SYMBOL(prt_human_readable_u64);

/**
 * prt_human_readable_s64 - Print out a s64 in human readable units
 *
 * Units of 2^10 (default) or 10^3 are controlled via @buf->si_units
 */
void prt_human_readable_s64(struct printbuf *buf, s64 v)
{
	if (v < 0)
		prt_char(buf, '-');
	prt_human_readable_u64(buf, abs(v));
}
EXPORT_SYMBOL(prt_human_readable_s64);

/**
 * prt_units_u64 - Print out a u64 according to printbuf unit options
 *
 * Units are either raw (default), or human readable units (controlled via
 * @buf->human_readable_units)
 */
void prt_units_u64(struct printbuf *out, u64 v)
{
	if (out->human_readable_units)
		prt_human_readable_u64(out, v);
	else
		prt_printf(out, "%llu", v);
}
EXPORT_SYMBOL(prt_units_u64);

/**
 * prt_units_s64 - Print out a s64 according to printbuf unit options
 *
 * Units are either raw (default), or human readable units (controlled via
 * @buf->human_readable_units)
 */
void prt_units_s64(struct printbuf *out, s64 v)
{
	if (v < 0)
		prt_char(out, '-');
	prt_units_u64(out, abs(v));
}
EXPORT_SYMBOL(prt_units_s64);
