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

#include <blkid.h>

#include "cmds.h"
#include "libbcachefs.h"
#include "libbcachefs/opts.h"
#include "tools-util.h"

#include "bcachefs.h"
#include "bcachefs_ioctl.h"

#include "init/dev.h"
#include "init/fs.h"

#include "journal/init.h"

#include "sb/members.h"
#include "sb/io.h"

static void device_add_usage(void)
{
	puts("bcachefs device add - add a device to an existing filesystem\n"
	     "Usage: bcachefs device add [OPTION]... filesystem device\n"
	     "\n"
	     "Options:\n");

	bch2_opts_usage(OPT_FORMAT|OPT_DEVICE);

	puts("  -l, --label=label            Disk label\n"
	     "  -f, --force                  Use device even if it appears to already be formatted\n"
	     "  -h, --help                   Display this help and exit\n"
	     "\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
}

static int cmd_device_add(int argc, char *argv[])
{
	static const struct option longopts[] = {
		{ "label",		required_argument,	NULL, 'l' },
		{ "force",		no_argument,		NULL, 'f' },
		{ "help",		no_argument,		NULL, 'h' },
		{ NULL }
	};
	struct dev_opts dev_opts	= dev_opts_default();
	bool force = false;

	while (true) {
		const struct bch_option *opt =
			bch2_cmdline_opt_parse(argc, argv, OPT_FORMAT|OPT_DEVICE);
		if (opt) {
			unsigned id = opt - bch2_opt_table;
			u64 v;
			struct printbuf err = PRINTBUF;
			int ret = bch2_opt_parse(NULL, opt, optarg, &v, &err);
			if (ret)
				die("invalid %s: %s", opt->attr.name, err.buf);

			if (opt->flags & OPT_DEVICE)
				bch2_opt_set_by_id(&dev_opts.opts, id, v);
			else
				die("got bch_opt of wrong type %s", opt->attr.name);

			continue;
		}

		int optid = getopt_long(argc, argv, "S:B:Dl:fh", longopts, NULL);
		if (optid == -1)
			break;

		switch (optid) {
		case 'l':
			dev_opts.label = strdup(optarg);
			break;
		case 'f':
			force = true;
			break;
		case 'h':
			device_add_usage();
			exit(EXIT_SUCCESS);
		case '?':
			exit(EXIT_FAILURE);
			break;
		}
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

	int ret = open_for_format(&dev_opts, 0, force) ?:
		bch2_format_for_device_add(&dev_opts,
			read_file_u64(fs.sysfs_fd, "options/block_size"),
			read_file_u64(fs.sysfs_fd, "options/btree_node_size"));
	if (ret)
		die("Error opening %s: %s", dev_opts.path, strerror(-ret));

	bchu_disk_add(fs, dev_opts.path);

	/* A whole bunch of nonsense to get blkid to update its cache, so
	 * mount can find the new device by UUID:
	 */
	blkid_cache cache = NULL;
	if (!blkid_get_cache(&cache, NULL)) {
		blkid_dev dev = blkid_get_dev(cache, dev_opts.path, BLKID_DEV_VERIFY);
		if (dev)
			blkid_verify(cache, dev);
		blkid_put_cache(cache);
	}

	return 0;
}

static void device_remove_usage(void)
{
	puts("bcachefs device_remove - remove a device from a filesystem\n"
	     "Usage:\n"
	     "  bcachefs device remove <device>|<devid> <path>\n"
	     "\n"
	     "Options:\n"
	     "  -f, --force                  Force removal, even if some data couldn't be migrated\n"
	     "  -F, --force-metadata         Force removal, even if some metadata couldn't be migrated\n"
	     "  -h, --help                   Display this help and exit\n"
	     "\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
	exit(EXIT_SUCCESS);
}

static int cmd_device_remove(int argc, char *argv[])
{
	static const struct option longopts[] = {
		{ "by-id",              no_argument, NULL, 'i' },
		{ "force",		no_argument, NULL, 'f' },
		{ "force-metadata",	no_argument, NULL, 'F' },
		{ "help",		no_argument, NULL, 'h' },
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
	     "  -h, --help                   Display this help and exit\n"
	     "\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
}

static int cmd_device_online(int argc, char *argv[])
{
	static const struct option longopts[] = {
		{ "help",		no_argument, NULL, 'h' },
		{ NULL }
	};
	int opt;

	while ((opt = getopt_long(argc, argv, "h", longopts, NULL)) != -1)
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
	     "  -f, --force                  Force, if data redundancy will be degraded\n"
	     "  -h, --help                   Display this help and exit\n"
	     "\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
}

static int cmd_device_offline(int argc, char *argv[])
{
	static const struct option longopts[] = {
		{ "force",		no_argument, NULL, 'f' },
		{ "help",		no_argument, NULL, 'h' },
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
	     "  -h, --help                   Display this help and exit\n"
	     "\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
}

static int evacuate_v0(struct bchfs_handle fs, unsigned dev_idx, const char *dev_path)
{
	struct bch_ioctl_dev_usage_v2 *u = bchu_dev_usage(fs, dev_idx);

	if (u->state == BCH_MEMBER_STATE_rw) {
		printf("Setting %s readonly\n", dev_path);
		bchu_disk_set_state(fs, dev_idx, BCH_MEMBER_STATE_ro, BCH_FORCE_IF_DEGRADED);
	}

	free(u);

	return bchu_data(fs, (struct bch_ioctl_data) {
		.op		= BCH_DATA_OP_migrate,
		.start_btree	= 0,
		.start_pos	= POS_MIN,
		.end_btree	= BTREE_ID_NR,
		.end_pos	= POS_MAX,
		.migrate.dev	= dev_idx,
	});
}

static int cmd_device_evacuate(int argc, char *argv[])
{
	static const struct option longopts[] = {
		{ "help",			no_argument,	NULL, 'h' },
		{ NULL }
	};
	int opt;

	while ((opt = getopt_long(argc, argv, "fh", longopts, NULL)) != -1)
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

	if (bcachefs_kernel_version() < bcachefs_metadata_version_reconcile)
		return evacuate_v0(fs, dev_idx, dev_path);

	printf("Setting %s failed\n", dev_path);
	bchu_disk_set_state(fs, dev_idx, BCH_MEMBER_STATE_failed, BCH_FORCE_IF_DEGRADED);

	while (true) {
		struct bch_ioctl_dev_usage_v2 *u = bchu_dev_usage(fs, dev_idx);

		u64 data = 0;
		for (unsigned type = 0; type < u->nr_data_types; type++)
			if (!data_type_is_empty(type) &&
			    !data_type_is_hidden(type))
				data += u->d[type].sectors;
		free(u);

		printf("\33[2K\r");
		CLASS(printbuf, buf)();
		prt_units_u64(&buf, data << 9);

		fputs(buf.buf, stdout);
		fflush(stdout);

		if (!data)
			return 0;

		sleep(1);
	}
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
	     "  -f, --force                  Force if data redundancy will be degraded\n"
	     "      --force-if-data-lost     Force if data will be lost\n"
	     "  -o, --offline                Set state of an offline device\n"
	     "  -h, --help                   Display this help and exit\n"
	     "\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
	exit(EXIT_SUCCESS);
}

static int cmd_device_set_state(int argc, char *argv[])
{
	static const struct option longopts[] = {
		{ "force",			no_argument,	NULL, 'f' },
		{ "force-if-data-lost",		no_argument,	NULL, 'F' },
		{ "offline",			no_argument,	NULL, 'o' },
		{ "help",			no_argument,	NULL, 'h' },
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
		if (by_id)
			die("Cannot specify offline device by id");

		struct bch_opts opts = bch2_opts_empty();
		opt_set(opts, nostart,	true);
		opt_set(opts, degraded, BCH_DEGRADED_very);

		struct bch_sb_handle sb = { NULL };
		int ret = bch2_read_super(dev_str, &opts, &sb);
		if (ret)
			die("error opening %s: %s", dev_str, bch2_err_str(ret));

		unsigned dev_idx = sb.sb->dev_idx;
		bch2_free_super(&sb);

		/* scan for all devices in fs */
		darray_const_str devs = get_or_split_cmdline_devs(1, &dev_str);

		struct bch_fs *c = bch2_fs_open(&devs, &opts);
		ret = PTR_ERR_OR_ZERO(c);
		if (ret)
			die("Error opening filesystem: %s", bch2_err_str(ret));

		scoped_guard(mutex, &c->sb_lock) {
			struct bch_member *m = bch2_members_v2_get_mut(c->disk_sb.sb, dev_idx);

			SET_BCH_MEMBER_STATE(m, new_state);

			bch2_write_super(c);
		}
		bch2_fs_stop(c);
		return ret;
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
	     "  -h, --help                   Display this help and exit\n"
	     "\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
	exit(EXIT_SUCCESS);
}

static int cmd_device_resize(int argc, char *argv[])
{
	static const struct option longopts[] = {
		{ "help",			no_argument, NULL, 'h' },
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
		size = get_size(dev_fd);
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

		struct bch_member m = bch2_sb_member_get(sb, idx);

		u64 nbuckets = size / BCH_MEMBER_BUCKET_SIZE(&m);

		if (nbuckets < le64_to_cpu(m.nbuckets))
			die("Shrinking not supported yet");

		printf("resizing %s to %llu buckets\n", dev, nbuckets);
		bchu_disk_resize(fs, idx, nbuckets);
	} else {
		printf("Doing offline resize of %s\n", dev);

		darray_const_str devs = {};
		darray_push(&devs, dev);

		struct bch_opts opts = bch2_opts_empty();
		struct bch_fs *c = bch2_fs_open(&devs, &opts);
		if (IS_ERR(c))
			die("error opening %s: %s", dev, bch2_err_str(PTR_ERR(c)));

		struct bch_dev *resize = NULL;

		for_each_online_member(c, ca, 0) {
			if (resize)
				die("confused: more than one online device?");
			resize = ca;
			enumerated_ref_get(&resize->io_ref[READ], 0);
		}

		u64 nbuckets = size / resize->mi.bucket_size;

		if (nbuckets < le64_to_cpu(resize->mi.nbuckets))
			die("Shrinking not supported yet");

		printf("resizing %s to %llu buckets\n", dev, nbuckets);
		CLASS(printbuf, err)();
		int ret = bch2_dev_resize(c, resize, nbuckets, &err);
		if (ret)
			fprintf(stderr, "resize error: %s\n%s", bch2_err_str(ret), err.buf);

		enumerated_ref_put(&resize->io_ref[READ], 0);
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
	     "  -h, --help                   Display this help and exit\n"
	     "\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
	exit(EXIT_SUCCESS);
}

static int cmd_device_resize_journal(int argc, char *argv[])
{
	static const struct option longopts[] = {
		{ "help",			no_argument, NULL, 'h' },
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

		struct bch_member m = bch2_sb_member_get(sb, idx);

		u64 nbuckets = size / BCH_MEMBER_BUCKET_SIZE(&m);

		printf("resizing journal on %s to %llu buckets\n", dev, nbuckets);
		bchu_disk_resize_journal(fs, idx, nbuckets);
	} else {
		printf("%s is offline - starting:\n", dev);

		darray_const_str devs = {};
		darray_push(&devs, dev);

		struct bch_opts opts = bch2_opts_empty();
		struct bch_fs *c = bch2_fs_open(&devs, &opts);
		if (IS_ERR(c))
			die("error opening %s: %s", dev, bch2_err_str(PTR_ERR(c)));

		struct bch_dev *resize = NULL;

		for_each_online_member(c, ca, 0) {
			if (resize)
				die("confused: more than one online device?");
			resize = ca;
			enumerated_ref_get(&resize->io_ref[READ], 0);
		}

		u64 nbuckets = size / le16_to_cpu(resize->mi.bucket_size);

		printf("resizing journal on %s to %llu buckets\n", dev, nbuckets);
		int ret = bch2_set_nr_journal_buckets(c, resize, nbuckets);
		if (ret)
			fprintf(stderr, "resize error: %s\n", bch2_err_str(ret));

		enumerated_ref_put(&resize->io_ref[READ], 0);
		bch2_fs_stop(c);
	}
	return 0;
}

static int device_usage(void)
{
       puts("bcachefs device - manage devices within a running filesystem\n"
            "Usage: bcachefs device <CMD> [OPTION]\n"
            "\n"
            "Commands:\n"
            "  add                          Add a new device to an existing filesystem\n"
            "  remove                       Remove a device from an existing filesystem\n"
            "  online                       Re-add an existing member to a filesystem\n"
            "  offline                      Take a device offline, without removing it\n"
            "  evacuate                     Migrate data off a specific device\n"
            "  set-state                    Mark a device as failed\n"
            "  resize                       Resize filesystem on a device\n"
            "  resize-journal               Resize journal on a device\n"
            "\n"
            "Report bugs to <linux-bcachefs@vger.kernel.org>");
       return 0;
}

int device_cmds(int argc, char *argv[])
{
	char *cmd = pop_cmd(&argc, argv);

	if (argc < 1)
		return device_usage();
	if (!strcmp(cmd, "add"))
		return cmd_device_add(argc, argv);
	if (!strcmp(cmd, "remove"))
		return cmd_device_remove(argc, argv);
	if (!strcmp(cmd, "online"))
		return cmd_device_online(argc, argv);
	if (!strcmp(cmd, "offline"))
		return cmd_device_offline(argc, argv);
	if (!strcmp(cmd, "evacuate"))
		return cmd_device_evacuate(argc, argv);
	if (!strcmp(cmd, "set-state"))
		return cmd_device_set_state(argc, argv);
	if (!strcmp(cmd, "resize"))
		return cmd_device_resize(argc, argv);
	if (!strcmp(cmd, "resize-journal"))
		return cmd_device_resize_journal(argc, argv);

	device_usage();
	return -EINVAL;
}
