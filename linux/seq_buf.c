// SPDX-License-Identifier: GPL-2.0
/*
 * seq_buf.c
 *
 * Copyright (C) 2014 Red Hat Inc, Steven Rostedt <srostedt@redhat.com>
 *
 * The seq_buf is a handy tool that allows you to pass a descriptor around
 * to a buffer that other functions can write to. It is similar to the
 * seq_file functionality but has some differences.
 *
 * To use it, the seq_buf must be initialized with seq_buf_init().
 * This will set up the counters within the descriptor. You can call
 * seq_buf_init() more than once to reset the seq_buf to start
 * from scratch.
 */
#include <linux/seq_buf.h>
#include <stdio.h>

/**
 * seq_buf_can_fit - can the new data fit in the current buffer?
 * @s: the seq_buf descriptor
 * @len: The length to see if it can fit in the current buffer
 *
 * Returns true if there's enough unused space in the seq_buf buffer
 * to fit the amount of new data according to @len.
 */
static bool seq_buf_can_fit(struct seq_buf *s, size_t len)
{
	return s->len + len <= s->size;
}

/**
 * seq_buf_vprintf - sequence printing of information.
 * @s: seq_buf descriptor
 * @fmt: printf format string
 * @args: va_list of arguments from a printf() type function
 *
 * Writes a vnprintf() format into the sequencce buffer.
 *
 * Returns zero on success, -1 on overflow.
 */
int seq_buf_vprintf(struct seq_buf *s, const char *fmt, va_list args)
{
	int len;

	WARN_ON(s->size == 0);

	if (s->len < s->size) {
		len = vsnprintf(s->buffer + s->len, s->size - s->len, fmt, args);
		if (s->len + len < s->size) {
			s->len += len;
			return 0;
		}
	}
	seq_buf_set_overflow(s);
	return -1;
}

/**
 * seq_buf_printf - sequence printing of information
 * @s: seq_buf descriptor
 * @fmt: printf format string
 *
 * Writes a printf() format into the sequence buffer.
 *
 * Returns zero on success, -1 on overflow.
 */
int seq_buf_printf(struct seq_buf *s, const char *fmt, ...)
{
	va_list ap;
	int ret;

	va_start(ap, fmt);
	ret = seq_buf_vprintf(s, fmt, ap);
	va_end(ap);

	return ret;
}

/**
 * seq_buf_puts - sequence printing of simple string
 * @s: seq_buf descriptor
 * @str: simple string to record
 *
 * Copy a simple string into the sequence buffer.
 *
 * Returns zero on success, -1 on overflow
 */
int seq_buf_puts(struct seq_buf *s, const char *str)
{
	size_t len = strlen(str);

	WARN_ON(s->size == 0);

	/* Add 1 to len for the trailing null byte which must be there */
	len += 1;

	if (seq_buf_can_fit(s, len)) {
		memcpy(s->buffer + s->len, str, len);
		/* Don't count the trailing null byte against the capacity */
		s->len += len - 1;
		return 0;
	}
	seq_buf_set_overflow(s);
	return -1;
}

/**
 * seq_buf_putc - sequence printing of simple character
 * @s: seq_buf descriptor
 * @c: simple character to record
 *
 * Copy a single character into the sequence buffer.
 *
 * Returns zero on success, -1 on overflow
 */
int seq_buf_putc(struct seq_buf *s, unsigned char c)
{
	WARN_ON(s->size == 0);

	if (seq_buf_can_fit(s, 1)) {
		s->buffer[s->len++] = c;
		return 0;
	}
	seq_buf_set_overflow(s);
	return -1;
}

/**
 * seq_buf_putmem - write raw data into the sequenc buffer
 * @s: seq_buf descriptor
 * @mem: The raw memory to copy into the buffer
 * @len: The length of the raw memory to copy (in bytes)
 *
 * There may be cases where raw memory needs to be written into the
 * buffer and a strcpy() would not work. Using this function allows
 * for such cases.
 *
 * Returns zero on success, -1 on overflow
 */
int seq_buf_putmem(struct seq_buf *s, const void *mem, unsigned int len)
{
	WARN_ON(s->size == 0);

	if (seq_buf_can_fit(s, len)) {
		memcpy(s->buffer + s->len, mem, len);
		s->len += len;
		return 0;
	}
	seq_buf_set_overflow(s);
	return -1;
}
