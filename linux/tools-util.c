#include <errno.h>
#include <fcntl.h>
#include <linux/fs.h>
#include <signal.h>
#include <stdbool.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <sys/types.h>
#include <unistd.h>

#include <uuid/uuid.h>

#include "tools-util.h"

void die(const char *fmt, ...)
{
	va_list args;

	va_start(args, fmt);
	vfprintf(stderr, fmt, args);
	va_end(args);
	fputc('\n', stderr);

	_exit(EXIT_FAILURE);
}

char *vmprintf(const char *fmt, va_list args)
{
	char *str;
	int ret = vasprintf(&str, fmt, args);
	if (ret < 0)
		die("insufficient memory");

	return str;
}

char *mprintf(const char *fmt, ...)
{
	va_list args;
	va_start(args, fmt);
	char *str = vmprintf(fmt, args);
	va_end(args);

	return str;
}

struct stat xfstat(int fd)
{
	struct stat stat;
	if (fstat(fd, &stat))
		die("stat error: %m");
	return stat;
}

/* File parsing (i.e. sysfs) */

char *read_file_str(int dirfd, const char *path)
{
	int fd = xopenat(dirfd, path, O_RDONLY);
	ssize_t len = xfstat(fd).st_size;

	char *buf = xmalloc(len + 1);

	len = read(fd, buf, len);
	if (len < 0)
		die("read error: %m");

	buf[len] = '\0';
	if (len && buf[len - 1] == '\n')
		buf[len - 1] = '\0';
	if (!strlen(buf)) {
		free(buf);
		buf = NULL;
	}

	xclose(fd);

	return buf;
}

u64 read_file_u64(int dirfd, const char *path)
{
	char *buf = read_file_str(dirfd, path);
	char *end;

	errno = 0;
	u64 v = buf ? strtoull(buf, &end, 10) : 0;
	if (!buf || errno || end == buf || *end)
		die("read_file_u64: error parsing %s (got %s)", path, buf ?: "");
	free(buf);
	return v;
}

#include <sys/syscall.h>
#ifndef SYS_getrandom
#include <linux/bug.h>

/* getrandom()'s, with no syscall for it: include/linux/random.h */
int urandom_fd;

__attribute__((constructor(101)))
static void rand_init(void)
{
	urandom_fd = open("/dev/urandom", O_RDONLY);
	BUG_ON(urandom_fd < 0);
}
#endif

/*
 * Defined in the tools binary (src/http.rs), and called by
 * os/linux_user/debugfs.rs. A weak empty definition lets other links of this
 * archive, such as the bcachefs-kernel crate tests, resolve the symbol. The
 * tools binary's strong definition overrides it.
 */
__attribute__((weak)) void bch2_start_http_lazy(void) {}
