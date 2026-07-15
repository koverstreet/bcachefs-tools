#ifndef _TOOLS_UTIL_H
#define _TOOLS_UTIL_H

#include <errno.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>

#include <linux/bug.h>
#include <linux/byteorder.h>
#include <linux/kernel.h>
#include <linux/log2.h>
#include <linux/string.h>
#include <linux/types.h>
#include <linux/uuid.h>

#include "bcachefs.h"

#define noreturn __attribute__((noreturn))

void die(const char *, ...)
	__attribute__ ((format (printf, 1, 2))) noreturn;

void bch2_install_fatal_signal_handlers(void);

char *vmprintf(const char *fmt, va_list args)
	__attribute__ ((format (printf, 1, 0)));

char *mprintf(const char *, ...)
	__attribute__ ((format (printf, 1, 2)));

struct stat xfstat(int);

static inline void *xmalloc(size_t size)
{
	void *p = malloc(size);

	if (!p)
		die("insufficient memory");

	memset(p, 0, size);
	return p;
}

#define xopenat(_dirfd, _path, ...)					\
({									\
	int _fd = openat((_dirfd), (_path), __VA_ARGS__);		\
	if (_fd < 0)							\
		die("Error opening %s: %m", (_path));			\
	_fd;								\
})

#define xioctl(_fd, _nr, ...)						\
({									\
	int _ret = ioctl((_fd), (_nr), ##__VA_ARGS__);			\
	if (_ret < 0)							\
		die(#_nr " ioctl error: %m");				\
	_ret;								\
})

#define xclose(_fd)							\
do {									\
	if (close(_fd))							\
		die("error closing fd: %m at %s:%u", __FILE__, __LINE__);\
} while (0)

char *read_file_str(int, const char *);
u64 read_file_u64(int, const char *);

void blkid_check(int fd, const char *path, bool force);

bool ask_yn(void);

/* Avoid conflicts with libblkid's crc32 function in static builds */
#define crc32c bch_crc32c
u32 crc32c(u32, const void *, size_t);

#endif /* _TOOLS_UTIL_H */
