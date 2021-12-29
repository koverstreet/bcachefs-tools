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

int device_usage(void)
{
       puts("bcachefs device - manage devices within a running filesystem\n"
            "Usage: bcachefs device <CMD> [OPTION]\n"
            "\n"
            "Commands:\n"
            "  add                     add a new device to an existing filesystem\n"
            "  remove                  remove a device from an existing filesystem\n"
            "  online                  re-add an existing member to a filesystem\n"
            "  offline                 take a device offline, without removing it\n"
            "  evacuate                migrate data off a specific device\n"
            "  set-state               mark a device as failed\n"
            "  resize                  resize filesystem on a device\n"
            "  resize-journal          resize journal on a device\n"
            "\n"
            "Report bugs to <linux-bcachefs@vger.kernel.org>");
       return 0;
}

static void device_add_usage(void)
{
	puts("bcachefs device add - add a device to an existing filesystem\n"
	     "Usage: bcachefs device add [OPTION]... filesystem device\n"
	     "\n"
	     "Options:\n"
	     "  -S, --fs_size=size          Size of filesystem on device\n"
	     "  -B, --bucket=size           Bucket size\n"
	     "  -D, --discard               Enable discards\n"
	     "  -l, --label=label           Disk label\n"
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
		{ "label",		required_argument,	NULL, 'l' },
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
			break;
		case 'B':
			if (bch2_strtoull_h(optarg, &dev_opts.bucket_size))
				die("bad bucket_size %s", optarg);
			break;
		case 'D':
			dev_opts.discard = true;
			break;
		case 'l':
			dev_opts.label = strdup(optarg);
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

	dev_opts.path = arg_pop();
	if (!dev_opts.path)
		die("Please supply a device");

	if (argc)
		die("too many arguments");

	struct bchfs_handle fs = bcache_fs_open(fs_path);

	dev_opts.fd = open_for_format(dev_opts.path, force);

	struct bch_opt_strs fs_opt_strs;
	memset(&fs_opt_strs, 0, sizeof(fs_opt_strs));

	struct bch_opts fs_opts = bch2_parse_opts(fs_opt_strs);

	opt_set(fs_opts, block_size,
		read_file_u64(fs.sysfs_fd, "options/block_size"));
	opt_set(fs_opts, btree_node_size,
		read_file_u64(fs.sysfs_fd, "options/btree_node_size"));

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
	     "  bcachefs device remove <device>|<devid> <path>\n"
	     "\n"
	     "Options:\n"
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
	int opt, flags = BCH_FORCE_IF_DEGRADED, dev_idx;

	while ((opt = getopt_long(argc, argv, "fh", longopts, NULL)) != -1)
		switch (opt) {
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

	char *dev_str = arg_pop();
	if (!dev_str)
		die("Please supply a device");

	char *end;
	dev_idx = strtoul(dev_str, &end, 10);
	if (*dev_str && !*end)
		by_id = true;

	char *fs_path = arg_pop();
	if (fs_path) {
		fs = bcache_fs_open(fs_path);

		if (!by_id) {
			dev_idx = bchu_dev_path_to_idx(fs, dev_str);
			if (dev_idx < 0)
				die("%s does not seem to be a member of %s",
				    dev_str, fs_path);
		}
	} else if (!by_id) {
		fs = bchu_fs_open_by_dev(dev_str, &dev_idx);
	} else {
		die("Filesystem path required when specifying device by id");
	}

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

	int dev_idx;
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

	int dev_idx;
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

	int dev_idx;
	struct bchfs_handle fs = bchu_fs_open_by_dev(dev_path, &dev_idx);

	struct bch_ioctl_dev_usage u = bchu_dev_usage(fs, dev_idx);

	if (u.state == BCH_MEMBER_STATE_rw) {
		printf("Setting %s readonly\n", dev_path);
		bchu_disk_set_state(fs, dev_idx, BCH_MEMBER_STATE_ro, 0);
	}

	return bchu_data(fs, (struct bch_ioctl_data) {
		.op		= BCH_DATA_OP_MIGRATE,
		.start_btree	= 0,
		.start_pos	= POS_MIN,
		.end_btree	= BTREE_ID_NR,
		.end_pos	= POS_MAX,
		.migrate.dev	= dev_idx,
	});
}

static void device_set_state_usage(void)
{
	puts("bcachefs device set-state\n"
	     "Usage: bcachefs device set-state <new-state> <device>|<devid> <path>\n"
	     "\n"
	     "<new-state>: one of rw, ro, failed or spare\n"
	     "<path>: path to mounted filesystem, optional unless specifying device by id\n"
	     "\n"
	     "Options:\n"
	     "  -f, --force		    Force, if data redundancy will be degraded\n"
	     "      --force-if-data-lost    Force, if data will be lost\n"
	     "  -o, --offline               Set state of an offline device\n"
	     "  -h, --help                  display this help and exit\n"
	     "Report bugs to <linux-bcache@vger.kernel.org>");
	exit(EXIT_SUCCESS);
}

int cmd_device_set_state(int argc, char *argv[])
{
	static const struct option longopts[] = {
		{ "force",			0, NULL, 'f' },
		{ "force-if-data-lost",		0, NULL, 'F' },
		{ "offline",			0, NULL, 'o' },
		{ "help",			0, NULL, 'h' },
		{ NULL }
	};
	struct bchfs_handle fs;
	bool by_id = false;
	int opt, flags = 0, dev_idx;
	bool offline = false;

	while ((opt = getopt_long(argc, argv, "foh", longopts, NULL)) != -1)
		switch (opt) {
		case 'f':
			flags |= BCH_FORCE_IF_DEGRADED;
			break;
		case 'F':
			flags |= BCH_FORCE_IF_DEGRADED;
			flags |= BCH_FORCE_IF_LOST;
			break;
		case 'o':
			offline = true;
			break;
		case 'h':
			device_set_state_usage();
		}
	args_shift(optind);

	char *new_state_str = arg_pop();
	if (!new_state_str)
		die("Please supply a device state");

	unsigned new_state = read_string_list_or_die(new_state_str,
					bch2_member_states, "device state");

	char *dev_str = arg_pop();
	if (!dev_str)
		die("Please supply a device");

	char *end;
	dev_idx = strtoul(dev_str, &end, 10);
	if (*dev_str && !*end)
		by_id = true;

	if (offline) {
		struct bch_opts opts = bch2_opts_empty();
		struct bch_sb_handle sb = { NULL };

		if (by_id)
			die("Cannot specify offline device by id");

		int ret = bch2_read_super(dev_str, &opts, &sb);
		if (ret)
			die("error opening %s: %s", dev_str, strerror(-ret));

		struct bch_member *m = bch2_sb_get_members(sb.sb)->members + sb.sb->dev_idx;

		SET_BCH_MEMBER_STATE(m, new_state);

		le64_add_cpu(&sb.sb->seq, 1);

		bch2_super_write(sb.bdev->bd_fd, sb.sb);
		bch2_free_super(&sb);
		return 0;
	}

	char *fs_path = arg_pop();
	if (fs_path) {
		fs = bcache_fs_open(fs_path);

		if (!by_id) {
			dev_idx = bchu_dev_path_to_idx(fs, dev_str);
			if (dev_idx < 0)
				die("%s does not seem to be a member of %s",
				    dev_str, fs_path);
		}
	} else if (!by_id) {
		fs = bchu_fs_open_by_dev(dev_str, &dev_idx);
	} else {
		die("Filesystem path required when specifying device by id");
	}

	bchu_disk_set_state(fs, dev_idx, new_state, flags);

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

		if (nbuckets < le64_to_cpu(m->nbuckets))
			die("Shrinking not supported yet");

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

		if (nbuckets < le64_to_cpu(resize->mi.nbuckets))
			die("Shrinking not supported yet");

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
	     "Usage: bcachefs device resize-journal device size\n"
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
		die("Please supply a journal size");
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
