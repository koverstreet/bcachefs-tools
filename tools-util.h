#ifndef _TOOLS_UTIL_H
#define _TOOLS_UTIL_H

#include <errno.h>
#include <mntent.h>
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
#include "ccan/darray/darray.h"

#define noreturn __attribute__((noreturn))

void die(const char *, ...)
	__attribute__ ((format (printf, 1, 2))) noreturn;
char *mprintf(const char *, ...)
	__attribute__ ((format (printf, 1, 2)));
void *xcalloc(size_t, size_t);
void *xmalloc(size_t);
void *xrealloc(void *, size_t);
void xpread(int, void *, size_t, off_t);
void xpwrite(int, const void *, size_t, off_t, const char *);
struct stat xfstatat(int, const char *, int);
struct stat xfstat(int);
struct stat xstat(const char *);

#define xopenat(_dirfd, _path, ...)					\
({									\
	int _fd = openat((_dirfd), (_path), __VA_ARGS__);		\
	if (_fd < 0)							\
		die("Error opening %s: %m", (_path));			\
	_fd;								\
})

#define xopen(...)	xopenat(AT_FDCWD, __VA_ARGS__)

#define xioctl(_fd, _nr, ...)						\
({									\
	int _ret = ioctl((_fd), (_nr), ##__VA_ARGS__);			\
	if (_ret < 0)							\
		die(#_nr " ioctl error: %m");				\
	_ret;								\
})

int printf_pad(unsigned pad, const char * fmt, ...);

enum units {
	BYTES,
	SECTORS,
	HUMAN_READABLE,
};

struct units_buf __pr_units(s64, enum units);

struct units_buf {
	char	b[20];
};

#define pr_units(_v, _u)	&(__pr_units(_v, _u).b[0])

char *read_file_str(int, const char *);
u64 read_file_u64(int, const char *);

ssize_t read_string_list_or_die(const char *, const char * const[],
				const char *);

u64 get_size(const char *, int);
unsigned get_blocksize(const char *, int);
int open_for_format(const char *, bool);

bool ask_yn(void);

struct range {
	u64		start;
	u64		end;
};

typedef darray(struct range) ranges;

static inline void range_add(ranges *data, u64 offset, u64 size)
{
	darray_append(*data, (struct range) {
		.start = offset,
		.end = offset + size
	});
}

void ranges_sort_merge(ranges *);
void ranges_roundup(ranges *, unsigned);
void ranges_rounddown(ranges *, unsigned);

struct hole_iter {
	ranges		r;
	size_t		idx;
	u64		end;
};

static inline struct range hole_iter_next(struct hole_iter *iter)
{
	struct range r = {
		.start	= iter->idx ? iter->r.item[iter->idx - 1].end : 0,
		.end	= iter->idx < iter->r.size
			? iter->r.item[iter->idx].start : iter->end,
	};

	BUG_ON(r.start > r.end);

	iter->idx++;
	return r;
}

#define for_each_hole(_iter, _ranges, _end, _i)				\
	for (_iter = (struct hole_iter) { .r = _ranges, .end = _end };	\
	     (_iter.idx <= _iter.r.size &&				\
	      (_i = hole_iter_next(&_iter), true));)

#include <linux/fiemap.h>

struct fiemap_iter {
	struct fiemap		f;
	struct fiemap_extent	fe[1024];
	unsigned		idx;
	int			fd;
};

static inline void fiemap_iter_init(struct fiemap_iter *iter, int fd)
{
	memset(iter, 0, sizeof(*iter));

	iter->f.fm_extent_count	= ARRAY_SIZE(iter->fe);
	iter->f.fm_length	= FIEMAP_MAX_OFFSET;
	iter->fd		= fd;
}

struct fiemap_extent fiemap_iter_next(struct fiemap_iter *);

#define fiemap_for_each(fd, iter, extent)				\
	for (fiemap_iter_init(&iter, fd);				\
	     (extent = fiemap_iter_next(&iter)).fe_length;)

char *strcmp_prefix(char *, const char *);

u32 crc32c(u32, const void *, size_t);

char *dev_to_name(dev_t);
char *dev_to_path(dev_t);
struct mntent *dev_to_mount(char *);
int dev_mounted(char *);

#define args_shift(_nr)							\
do {									\
	unsigned _n = min((_nr), argc);					\
	argc -= _n;							\
	argv += _n;							\
} while (0)

#define arg_pop()							\
({									\
	char *_ret = argc ? argv[0] : NULL;				\
	if (_ret)							\
		args_shift(1);						\
	_ret;								\
})

struct bpos bpos_parse(char *);

#endif /* _TOOLS_UTIL_H */
