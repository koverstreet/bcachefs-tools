#include <errno.h>
#include <fcntl.h>
#include <getopt.h>
#include <libgen.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>

#include "libbcachefs/bcachefs.h"
#include "libbcachefs/bcachefs_ioctl.h"
#include "cmds.h"
#include "libbcachefs.h"
#include "libbcachefs/opts.h"
#include "tools-util.h"

int subvolume_usage(void)
{
	puts("bcachefs subvolume - manage subvolumes and snapshots\n"
	     "Usage: bcachefs subvolume <CMD> [OPTION]\n"
	     "\n"
	     "Commands:\n"
	     "  create                  create a subvolume\n"
	     "  delete                  delete a subvolume\n"
	     "  snapshot                create a snapshot\n"
	     "\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
	return 0;
}

static void subvolume_create_usage(void)
{
	puts("bcachefs subvolume create - create a new subvolume\n"
	     "Usage: bcachefs subvolume create [OPTION]... path\n"
	     "\n"
	     "Options:\n"
	     "  -h, --help                  Display this help and exit\n"
	     "\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
}

int cmd_subvolume_create(int argc, char *argv[])
{
	static const struct option longopts[] = {
		{ "help",		no_argument,		NULL, 'h' },
		{ NULL }
	};
	char *path;
	int opt;

	while ((opt = getopt_long(argc, argv, "h", longopts, NULL)) != -1)
		switch (opt) {
		case 'h':
			subvolume_create_usage();
			exit(EXIT_SUCCESS);
		}
	args_shift(optind);

	while ((path = arg_pop())) {
		char *dir = dirname(strdup(path));

		struct bchfs_handle fs = bcache_fs_open(dir);

		struct bch_ioctl_subvolume i = {
			.dirfd		= AT_FDCWD,
			.mode		= 0777,
			.dst_ptr	= (unsigned long)path,
		};

		xioctl(fs.ioctl_fd, BCH_IOCTL_SUBVOLUME_CREATE, &i);
		bcache_fs_close(fs);
	}

	return 0;
}

static void subvolume_delete_usage(void)
{
	puts("bcachefs subvolume delete - delete an existing subvolume\n"
	     "Usage: bcachefs subvolume delete [OPTION]... path\n"
	     "\n"
	     "Options:\n"
	     "  -h, --help                  Display this help and exit\n"
	     "\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
}

int cmd_subvolume_delete(int argc, char *argv[])
{
	static const struct option longopts[] = {
		{ "help",		no_argument,		NULL, 'h' },
		{ NULL }
	};
	char *path;
	int opt;

	while ((opt = getopt_long(argc, argv, "h", longopts, NULL)) != -1)
		switch (opt) {
		case 'h':
			subvolume_delete_usage();
			exit(EXIT_SUCCESS);
		}
	args_shift(optind);

	while ((path = arg_pop())) {
		char *dir = dirname(strdup(path));

		struct bchfs_handle fs = bcache_fs_open(dir);

		struct bch_ioctl_subvolume i = {
			.dirfd		= AT_FDCWD,
			.mode		= 0777,
			.dst_ptr	= (unsigned long)path,
		};

		xioctl(fs.ioctl_fd, BCH_IOCTL_SUBVOLUME_DESTROY, &i);
		bcache_fs_close(fs);
	}

	return 0;
}

static void snapshot_create_usage(void)
{
	puts("bcachefs subvolume snapshot - create a snapshot \n"
	     "Usage: bcachefs subvolume snapshot [OPTION]... <source> <dest>\n"
	     "\n"
	     "Create a snapshot of <source> at <dest>. If specified, <source> must be a subvolume;\n"
	     "if not specified the snapshot will be of the subvolme containing <dest>.\n"
	     "Options:\n"
	     "  -r                          Make snapshot read only\n"
	     "  -h, --help                  Display this help and exit\n"
	     "\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
}

int cmd_subvolume_snapshot(int argc, char *argv[])
{
	static const struct option longopts[] = {
		{ "help",		no_argument,		NULL, 'h' },
		{ NULL }
	};
	unsigned flags = BCH_SUBVOL_SNAPSHOT_CREATE;
	int opt;

	while ((opt = getopt_long(argc, argv, "rh", longopts, NULL)) != -1)
		switch (opt) {
		case 'r':
			flags |= BCH_SUBVOL_SNAPSHOT_RO;
			break;
		case 'h':
			snapshot_create_usage();
			exit(EXIT_SUCCESS);
		}
	args_shift(optind);

	char *src = arg_pop();
	char *dst = arg_pop();

	if (argc)
		die("Too many arguments");

	if (!dst)
		swap(src, dst);
	if (!dst)
		die("Please specify a path to create");

	char *dir = dirname(strdup(dst));

	struct bchfs_handle fs = bcache_fs_open(dir);

	struct bch_ioctl_subvolume i = {
		.flags		= flags,
		.dirfd		= AT_FDCWD,
		.mode		= 0777,
		.src_ptr	= (unsigned long)src,
		.dst_ptr	= (unsigned long)dst,
	};

	xioctl(fs.ioctl_fd, BCH_IOCTL_SUBVOLUME_CREATE, &i);
	bcache_fs_close(fs);
	return 0;
}
