/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_THREAD_WITH_FILE_H
#define _BCACHEFS_THREAD_WITH_FILE_H

#include "util/darray.h"
#include "util/thread_with_file_gen.h"

#include "util/thread_with_file_defs_gen.h"

void bch2_thread_with_file_exit(struct thread_with_file *);
int bch2_run_thread_with_file(struct thread_with_file *,
			      const struct file_operations *,
			      int (*fn)(void *));

void bch2_thread_with_stdio_init(struct thread_with_stdio *,
				 const struct thread_with_stdio_ops *);
void bch2_thread_with_stdio_done(struct thread_with_stdio *);
int __bch2_run_thread_with_stdio(struct thread_with_stdio *);
int bch2_run_thread_with_stdio(struct thread_with_stdio *,
			       const struct thread_with_stdio_ops *);
int bch2_stdio_redirect_get_fd(struct thread_with_stdio *,
			       const struct thread_with_stdio_ops *,
			       struct file **);
int bch2_run_thread_with_stdout(struct thread_with_stdio *,
				const struct thread_with_stdio_ops *);
int bch2_stdio_redirect_read(struct stdio_redirect *, char *, size_t);

int bch2_stdio_redirect_readline_timeout(struct stdio_redirect *, darray_char *, unsigned long);
int bch2_stdio_redirect_readline(struct stdio_redirect *, darray_char *);

ssize_t bch2_stdio_redirect_write(struct stdio_redirect *, bool, const char *, size_t);
__printf(3, 0) ssize_t bch2_stdio_redirect_vprintf(struct stdio_redirect *, bool, const char *, va_list);
__printf(3, 4) ssize_t bch2_stdio_redirect_printf(struct stdio_redirect *, bool, const char *, ...);

#endif /* _BCACHEFS_THREAD_WITH_FILE_H */
