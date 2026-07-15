/* SPDX-License-Identifier: GPL-2.0 */
/*
 * (C) 2022-2024 Kent Overstreet <kent.overstreet@linux.dev>
 */
#ifndef _LINUX_THREAD_WITH_FILE_H
#define _LINUX_THREAD_WITH_FILE_H

struct stdio_redirect;

__printf(3, 0)
static inline void stdio_redirect_vprintf(struct stdio_redirect *s, bool nonblocking, const char *msg, va_list args) {}
__printf(3, 4)
static inline void stdio_redirect_printf(struct stdio_redirect *s, bool nonblocking, const char *msg, ...) {}

#endif /* _LINUX_THREAD_WITH_FILE_H */
