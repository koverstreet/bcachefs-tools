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
#include "libbcachefs/journal.h"
#include "libbcachefs/super-io.h"
#include "cmds.h"
#include "libbcachefs.h"
#include "libbcachefs/opts.h"
#include "tools-util.h"

static void device_add_usage(void)
{
	puts("bcachefs device add - add a device to an existing filesystem\n"
	     "Usage: bcachefs device add [OPTION]... filesystem device\n"
	     "\n"
	     "Options:\n"
	     "  -S, --fs_size=size          Size of filesystem on device\n"
	     "  -B, --bucket=size           Bucket size\n"
	     "  -D, --discard               Enable discards\n"
	     "  -g, --group=group           Disk group\n"
	     "  -f, --force                 Use device even if it appears to already be formatted\n"
	     "  -h, --help                  Display this help and exit\n"
	     "\n"
	     "Report bugs to <linux-bcache@vger.kernel.org>");
}

int cmd_device_add(int argc, char *argv[])
{
	static const struct option longopts[] = {
		{ "fs_size",		required_argument,	NULL, 'S' },
		{ "bucket",		required_argument,	NULL, 'B' },
		{ "discard",		no_argument,		NULL, 'D' },
		{ "group",		required_argument,	NULL, 'g' },
		{ "force",		no_argument,		NULL, 'f' },
		{ "help",		no_argument,		NULL, 'h' },
		{ NULL }
	};
	struct format_opts format_opts	= format_opts_default();
	struct dev_opts dev_opts	= dev_opts_default();
	bool force = false;
	int opt;

	while ((opt = getopt_long(argc, argv, "t:fh",
				  longopts, NULL)) != -1)
		switch (opt) {
		case 'S':
			if (bch2_strtoull_h(optarg, &dev_opts.size))
				die("invalid filesystem size");

			dev_opts.size >>= 9;
			break;
		case 'B':
			dev_opts.bucket_size =
				hatoi_validate(optarg, "bucket size");
			break;
		case 'D':
			dev_opts.discard = true;
			break;
		case 'g':
			dev_opts.group = strdup(optarg);
			break;
		case 'f':
			force = true;
			break;
		case 'h':
			device_add_usage();
			exit(EXIT_SUCCESS);
		}
	args_shift(optind);

	char *fs_path = arg_pop();
	if (!fs_path)
		die("Please supply a filesystem");

	char *dev_path = arg_pop();
	if (!dev_path)
		die("Please supply a device");

	if (argc)
		die("too many arguments");

	struct bchfs_handle fs = bcache_fs_open(fs_path);

	dev_opts.path = dev_path;
	dev_opts.fd = open_for_format(dev_opts.path, force);

	struct bch_opt_strs fs_opt_strs;
	memset(&fs_opt_strs, 0, sizeof(fs_opt_strs));

	struct bch_opts fs_opts = bch2_parse_opts(fs_opt_strs);

	opt_set(fs_opts, block_size,
		read_file_u64(fs.sysfs_fd, "block_size") >> 9);
	opt_set(fs_opts, btree_node_size,
		read_file_u64(fs.sysfs_fd, "btree_node_size") >> 9);

	struct bch_sb *sb = bch2_format(fs_opt_strs,
					fs_opts,
					format_opts,
					&dev_opts, 1);
	free(sb);
	fsync(dev_opts.fd);
	close(dev_opts.fd);

	bchu_disk_add(fs, dev_opts.path);
	return 0;
}

static void device_remove_usage(void)
{
	puts("bcachefs device_remove - remove a device from a filesystem\n"
	     "Usage:\n"
	     "  bcachefs device remove device\n"
	     "  bcachefs device remove --by-id path devid\n"
	     "\n"
	     "Options:\n"
	     "  -i, --by-id                 Remove device by device id\n"
	     "  -f, --force		    Force removal, even if some data\n"
	     "                              couldn't be migrated\n"
	     "  -F, --force-metadata	    Force removal, even if some metadata\n"
	     "                              couldn't be migrated\n"
	     "  -h, --help                  display this help and exit\n"
	     "Report bugs to <linux-bcache@vger.kernel.org>");
	exit(EXIT_SUCCESS);
}

int cmd_device_remove(int argc, char *argv[])
{
	static const struct option longopts[] = {
		{ "by-id",              0, NULL, 'i' },
		{ "force",		0, NULL, 'f' },
		{ "force-metadata",	0, NULL, 'F' },
		{ "help",		0, NULL, 'h' },
		{ NULL }
	};
	struct bchfs_handle fs;
	bool by_id = false;
	int opt, flags = BCH_FORCE_IF_DEGRADED;
	unsigned dev_idx;

	while ((opt = getopt_long(argc, argv, "fh", longopts, NULL)) != -1)
		switch (opt) {
		case 'i':
			by_id = true;
			break;
		case 'f':
			flags |= BCH_FORCE_IF_DATA_LOST;
			break;
		case 'F':
			flags |= BCH_FORCE_IF_METADATA_LOST;
			break;
		case 'h':
			device_remove_usage();
		}
	args_shift(optind);

	if (by_id) {
		char *path = arg_pop();
		if (!path)
			die("Please supply filesystem to remove device from");

		char *dev_str = arg_pop();
		if (!dev_str)
			die("Please supply device id");

		errno = 0;
		dev_idx = strtoul(dev_str, NULL, 10);
		if (errno)
			die("Error parsing device id: %m");

		fs = bcache_fs_open(path);
	} else {
		char *dev = arg_pop();
		if (!dev)
			die("Please supply a device to remove");

		fs = bchu_fs_open_by_dev(dev, &dev_idx);
	}

	if (argc)
		die("too many arguments");

	bchu_disk_remove(fs, dev_idx, flags);
	return 0;
}

static void device_online_usage(void)
{
	puts("bcachefs device online - readd a device to a running filesystem\n"
	     "Usage: bcachefs device online [OPTION]... device\n"
	     "\n"
	     "Options:\n"
	     "  -h, --help                  Display this help and exit\n"
	     "\n"
	     "Report bugs to <linux-bcache@vger.kernel.org>");
}

int cmd_device_online(int argc, char *argv[])
{
	int opt;

	while ((opt = getopt(argc, argv, "h")) != -1)
		switch (opt) {
		case 'h':
			device_online_usage();
			exit(EXIT_SUCCESS);
		}
	args_shift(optind);

	char *dev = arg_pop();
	if (!dev)
		die("Please supply a device");

	if (argc)
		die("too many arguments");

	unsigned dev_idx;
	struct bchfs_handle fs = bchu_fs_open_by_dev(dev, &dev_idx);
	bchu_disk_online(fs, dev);
	return 0;
}

static void device_offline_usage(void)
{
	puts("bcachefs device offline - take a device offline, without removing it\n"
	     "Usage: bcachefs device offline [OPTION]... device\n"
	     "\n"
	     "Options:\n"
	     "  -f, --force		    Force, if data redundancy will be degraded\n"
	     "  -h, --help                  Display this help and exit\n"
	     "\n"
	     "Report bugs to <linux-bcache@vger.kernel.org>");
}

int cmd_device_offline(int argc, char *argv[])
{
	static const struct option longopts[] = {
		{ "force",		0, NULL, 'f' },
		{ NULL }
	};
	int opt, flags = 0;

	while ((opt = getopt_long(argc, argv, "fh",
				  longopts, NULL)) != -1)
		switch (opt) {
		case 'f':
			flags |= BCH_FORCE_IF_DEGRADED;
			break;
		case 'h':
			device_offline_usage();
			exit(EXIT_SUCCESS);
		}
	args_shift(optind);

	char *dev = arg_pop();
	if (!dev)
		die("Please supply a device");

	if (argc)
		die("too many arguments");

	unsigned dev_idx;
	struct bchfs_handle fs = bchu_fs_open_by_dev(dev, &dev_idx);
	bchu_disk_offline(fs, dev_idx, flags);
	return 0;
}

static void device_evacuate_usage(void)
{
	puts("bcachefs device evacuate - move data off of a given device\n"
	     "Usage: bcachefs device evacuate [OPTION]... device\n"
	     "\n"
	     "Options:\n"
	     "  -h, --help                  Display this help and exit\n"
	     "\n"
	     "Report bugs to <linux-bcache@vger.kernel.org>");
}

int cmd_device_evacuate(int argc, char *argv[])
{
	int opt;

	while ((opt = getopt(argc, argv, "h")) != -1)
		switch (opt) {
		case 'h':
			device_evacuate_usage();
			exit(EXIT_SUCCESS);
		}
	args_shift(optind);

	char *dev_path = arg_pop();
	if (!dev_path)
		die("Please supply a device");

	if (argc)
		die("too many arguments");

	unsigned dev_idx;
	struct bchfs_handle fs = bchu_fs_open_by_dev(dev_path, &dev_idx);

	struct bch_ioctl_dev_usage u = bchu_dev_usage(fs, dev_idx);

	if (u.state == BCH_MEMBER_STATE_RW) {
		printf("Setting %s readonly\n", dev_path);
		bchu_disk_set_state(fs, dev_idx, BCH_MEMBER_STATE_RO, 0);
	}

	return bchu_data(fs, (struct bch_ioctl_data) {
		.op		= BCH_DATA_OP_MIGRATE,
		.start		= POS_MIN,
		.end		= POS_MAX,
		.migrate.dev	= dev_idx,
	});
}

static void device_set_state_usage(void)
{
	puts("bcachefs device set-state\n"
	     "Usage: bcachefs device set-state device new-state\n"
	     "\n"
	     "Options:\n"
	     "  -f, --force		    Force, if data redundancy will be degraded\n"
	     "  -o, --offline               Set state of an offline device\n"
	     "  -h, --help                  display this help and exit\n"
	     "Report bugs to <linux-bcache@vger.kernel.org>");
	exit(EXIT_SUCCESS);
}

int cmd_device_set_state(int argc, char *argv[])
{
	static const struct option longopts[] = {
		{ "force",			0, NULL, 'f' },
		{ "offline",			0, NULL, 'o' },
		{ "help",			0, NULL, 'h' },
		{ NULL }
	};
	int opt, flags = 0;
	bool offline = false;

	while ((opt = getopt_long(argc, argv, "foh", longopts, NULL)) != -1)
		switch (opt) {
		case 'f':
			flags |= BCH_FORCE_IF_DEGRADED;
			break;
		case 'o':
			offline = true;
			break;
		case 'h':
			device_set_state_usage();
		}
	args_shift(optind);

	char *dev_path = arg_pop();
	if (!dev_path)
		die("Please supply a device");

	char *new_state_str = arg_pop();
	if (!new_state_str)
		die("Please supply a device state");

	unsigned new_state = read_string_list_or_die(new_state_str,
					bch2_dev_state, "device state");

	if (!offline) {
		unsigned dev_idx;
		struct bchfs_handle fs = bchu_fs_open_by_dev(dev_path, &dev_idx);

		bchu_disk_set_state(fs, dev_idx, new_state, flags);

		bcache_fs_close(fs);
	} else {
		struct bch_opts opts = bch2_opts_empty();
		struct bch_sb_handle sb = { NULL };

		int ret = bch2_read_super(dev_path, &opts, &sb);
		if (ret)
			die("error opening %s: %s", dev_path, strerror(-ret));

		struct bch_member *m = bch2_sb_get_members(sb.sb)->members + sb.sb->dev_idx;

		SET_BCH_MEMBER_STATE(m, new_state);

		le64_add_cpu(&sb.sb->seq, 1);

		bch2_super_write(sb.bdev->bd_fd, sb.sb);
		bch2_free_super(&sb);
	}

	return 0;
}

static void device_resize_usage(void)
{
	puts("bcachefs device resize \n"
	     "Usage: bcachefs device resize device [ size ]\n"
	     "\n"
	     "Options:\n"
	     "  -h, --help                  display this help and exit\n"
	     "Report bugs to <linux-bcache@vger.kernel.org>");
	exit(EXIT_SUCCESS);
}

int cmd_device_resize(int argc, char *argv[])
{
	static const struct option longopts[] = {
		{ "help",			0, NULL, 'h' },
		{ NULL }
	};
	u64 size;
	int opt;

	while ((opt = getopt_long(argc, argv, "h", longopts, NULL)) != -1)
		switch (opt) {
		case 'h':
			device_resize_usage();
		}
	args_shift(optind);

	char *dev = arg_pop();
	if (!dev)
		die("Please supply a device to resize");

	int dev_fd = xopen(dev, O_RDONLY);

	char *size_arg = arg_pop();
	if (!size_arg)
		size = get_size(dev, dev_fd);
	else if (bch2_strtoull_h(size_arg, &size))
		die("invalid size");

	size >>= 9;

	if (argc)
		die("Too many arguments");

	struct stat dev_stat = xfstat(dev_fd);

	struct mntent *mount = dev_to_mount(dev);
	if (mount) {
		if (!S_ISBLK(dev_stat.st_mode))
			die("%s is mounted but isn't a block device?!", dev);

		printf("Doing online resize of %s\n", dev);

		struct bchfs_handle fs = bcache_fs_open(mount->mnt_dir);

		unsigned idx = bchu_disk_get_idx(fs, dev_stat.st_rdev);

		struct bch_sb *sb = bchu_read_super(fs, -1);
		if (idx >= sb->nr_devices)
			die("error reading superblock: dev idx >= sb->nr_devices");

		struct bch_sb_field_members *mi = bch2_sb_get_members(sb);
		if (!mi)
			die("error reading superblock: no member info");

		/* could also just read this out of sysfs... meh */
		struct bch_member *m = mi->members + idx;

		u64 nbuckets = size / le16_to_cpu(m->bucket_size);

		printf("resizing %s to %llu buckets\n", dev, nbuckets);
		bchu_disk_resize(fs, idx, nbuckets);
	} else {
		printf("Doing offline resize of %s\n", dev);

		struct bch_fs *c = bch2_fs_open(&dev, 1, bch2_opts_empty());
		if (IS_ERR(c))
			die("error opening %s: %s", dev, strerror(-PTR_ERR(c)));

		struct bch_dev *ca, *resize = NULL;
		unsigned i;

		for_each_online_member(ca, c, i) {
			if (resize)
				die("confused: more than one online device?");
			resize = ca;
			percpu_ref_get(&resize->io_ref);
		}

		u64 nbuckets = size / le16_to_cpu(resize->mi.bucket_size);

		printf("resizing %s to %llu buckets\n", dev, nbuckets);
		int ret = bch2_dev_resize(c, resize, nbuckets);
		if (ret)
			fprintf(stderr, "resize error: %s\n", strerror(-ret));

		percpu_ref_put(&resize->io_ref);
		bch2_fs_stop(c);
	}
	return 0;
}

static void device_resize_journal_usage(void)
{
	puts("bcachefs device resize-journal \n"
	     "Usage: bcachefs device resize-journal device [ size ]\n"
	     "\n"
	     "Options:\n"
	     "  -h, --help                  display this help and exit\n"
	     "Report bugs to <linux-bcache@vger.kernel.org>");
	exit(EXIT_SUCCESS);
}

int cmd_device_resize_journal(int argc, char *argv[])
{
	static const struct option longopts[] = {
		{ "help",			0, NULL, 'h' },
		{ NULL }
	};
	u64 size;
	int opt;

	while ((opt = getopt_long(argc, argv, "h", longopts, NULL)) != -1)
		switch (opt) {
		case 'h':
			device_resize_journal_usage();
		}
	args_shift(optind);

	char *dev = arg_pop();
	if (!dev)
		die("Please supply a device");

	int dev_fd = xopen(dev, O_RDONLY);

	char *size_arg = arg_pop();
	if (!size_arg)
		size = get_size(dev, dev_fd);
	else if (bch2_strtoull_h(size_arg, &size))
		die("invalid size");

	size >>= 9;

	if (argc)
		die("Too many arguments");

	struct stat dev_stat = xfstat(dev_fd);

	struct mntent *mount = dev_to_mount(dev);
	if (mount) {
		if (!S_ISBLK(dev_stat.st_mode))
			die("%s is mounted but isn't a block device?!", dev);

		struct bchfs_handle fs = bcache_fs_open(mount->mnt_dir);

		unsigned idx = bchu_disk_get_idx(fs, dev_stat.st_rdev);

		struct bch_sb *sb = bchu_read_super(fs, -1);
		if (idx >= sb->nr_devices)
			die("error reading superblock: dev idx >= sb->nr_devices");

		struct bch_sb_field_members *mi = bch2_sb_get_members(sb);
		if (!mi)
			die("error reading superblock: no member info");

		/* could also just read this out of sysfs... meh */
		struct bch_member *m = mi->members + idx;

		u64 nbuckets = size / le16_to_cpu(m->bucket_size);

		printf("resizing journal on %s to %llu buckets\n", dev, nbuckets);
		bchu_disk_resize_journal(fs, idx, nbuckets);
	} else {
		printf("%s is offline - starting:\n", dev);

		struct bch_fs *c = bch2_fs_open(&dev, 1, bch2_opts_empty());
		if (IS_ERR(c))
			die("error opening %s: %s", dev, strerror(-PTR_ERR(c)));

		struct bch_dev *ca, *resize = NULL;
		unsigned i;

		for_each_online_member(ca, c, i) {
			if (resize)
				die("confused: more than one online device?");
			resize = ca;
			percpu_ref_get(&resize->io_ref);
		}

		u64 nbuckets = size / le16_to_cpu(resize->mi.bucket_size);

		printf("resizing journal on %s to %llu buckets\n", dev, nbuckets);
		int ret = bch2_set_nr_journal_buckets(c, resize, nbuckets);
		if (ret)
			fprintf(stderr, "resize error: %s\n", strerror(-ret));

		percpu_ref_put(&resize->io_ref);
		bch2_fs_stop(c);
	}
	return 0;
}
