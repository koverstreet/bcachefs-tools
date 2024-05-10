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
#include "libbcachefs/bcachefs.h"
#include "libbcachefs/bbpos.h"
#include "libbcachefs/darray.h"

#define noreturn __attribute__((noreturn))

void die(const char *, ...)
	__attribute__ ((format (printf, 1, 2))) noreturn;
char *mprintf(const char *, ...)
	__attribute__ ((format (printf, 1, 2)));
void xpread(int, void *, size_t, off_t);
void xpwrite(int, const void *, size_t, off_t, const char *);
struct stat xfstatat(int, const char *, int);
struct stat xfstat(int);
struct stat xstat(const char *);

static inline void *xmalloc(size_t size)
{
	void *p = malloc(size);

	if (!p)
		die("insufficient memory");

	memset(p, 0, size);
	return p;
}

static inline void *xcalloc(size_t count, size_t size)
{
	void *p = calloc(count, size);

	if (!p)
		die("insufficient memory");

	return p;
}

static inline void *xrealloc(void *p, size_t size)
{
	p = realloc(p, size);
	if (!p)
		die("insufficient memory");

	return p;
}

static inline char *xstrdup(const char *s)
{
	void *p = strdup(s);
	if (!p)
		die("insufficient memory");

	return p;
}

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

void write_file_str(int, const char *, const char *);
char *read_file_str(int, const char *);
u64 read_file_u64(int, const char *);

ssize_t read_string_list_or_die(const char *, const char * const[],
				const char *);

u64 get_size(int);
unsigned get_blocksize(int);
struct dev_opts;
int open_for_format(struct dev_opts *, bool);

bool ask_yn(void);

struct range {
	u64		start;
	u64		end;
};

typedef DARRAY(struct range) ranges;

static inline void range_add(ranges *data, u64 offset, u64 size)
{
	darray_push(data, ((struct range) {
		.start = offset,
		.end = offset + size
	}));
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
		.start	= iter->idx ? iter->r.data[iter->idx - 1].end : 0,
		.end	= iter->idx < iter->r.nr
			? iter->r.data[iter->idx].start : iter->end,
	};

	BUG_ON(r.start > r.end);

	iter->idx++;
	return r;
}

#define for_each_hole(_iter, _ranges, _end, _i)				\
	for (_iter = (struct hole_iter) { .r = _ranges, .end = _end };	\
	     (_iter.idx <= _iter.r.nr &&				\
	      (_i = hole_iter_next(&_iter), true));)

#include <linux/fiemap.h>

struct fiemap_iter {
	struct fiemap		*f;
	unsigned		idx;
	int			fd;
};

static inline void fiemap_iter_init(struct fiemap_iter *iter, int fd)
{
	memset(iter, 0, sizeof(*iter));

	iter->f = xmalloc(sizeof(struct fiemap) +
			  sizeof(struct fiemap_extent) * 1024);

	iter->f->fm_extent_count	= 1024;
	iter->f->fm_length	= FIEMAP_MAX_OFFSET;
	iter->fd		= fd;
}

static inline void fiemap_iter_exit(struct fiemap_iter *iter)
{
	free(iter->f);
	memset(iter, 0, sizeof(*iter));
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
char *fd_to_dev_model(int);

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
struct bbpos bbpos_parse(char *);

struct bbpos_range {
	struct bbpos	start;
	struct bbpos	end;
};

struct bbpos_range bbpos_range_parse(char *);

darray_str get_or_split_cmdline_devs(int argc, char *argv[]);

#endif /* _TOOLS_UTIL_H */
